//! I/O scheduling primitives for the HDD-aware lane scheduler.
//!
//! This module is the foundation for the Spike A scheduler shell. It
//! introduces:
//!
//! - [`Lane`] — the traffic classes the engine routes I/O through.
//! - [`OpKind`] — read / write / fsync operation taxonomy.
//! - [`Scheduler`] — top-level enum dispatch between [`Scheduler::Passthrough`]
//!   (zero-overhead SSD path; default) and [`Scheduler::Laned`] (HDD opt-in).
//! - [`LanedScheduler`] — shell with per-lane atomic counters; service-time
//!   EWMA + bounded-outstanding ladder land in H1 (Day 16-25).
//!
//! At the Spike A level the scheduler is intentionally **observe-only**: the
//! `before_op` hook is a no-op, `after_op` accumulates op count + elapsed-us
//! per lane, and no actual throttling happens. The cycle's H1 implementation
//! plugs token-bucket capacities + reader-feedback backoff into the same
//! surface without changing call sites.
//!
//! The choice of enum dispatch over `Box<dyn IoScheduler>` is intentional:
//! the [`Scheduler::Passthrough`] arm compiles to an empty match arm that the
//! optimizer elides entirely, so the SSD path pays no runtime cost. A trait
//! object would force a vtable lookup per call, which exceeds the work
//! Passthrough does and would invalidate the SSD G5 ≤ 2 % gate.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Traffic classification for I/O operations. Encodes priority and (for
/// compaction) sub-priority by target level.
///
/// The priority order is:
///   1. [`Lane::UserIORead`]   (P0 — latency-critical, < 30 ms P50 HDD SLO)
///   2. [`Lane::WriterDurable`] (P1 — never preempted; group-commit fsync)
///   3. [`Lane::Flush`]         (P2 — yields to UserIORead)
///   4. [`Lane::Compaction`]    (P3 — yields to UserIORead and WriterDurable)
///
/// `Lane` is `Copy`, so passing it by value is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// User-facing reads (Q1-Q6 anchor lookups, MCP query). P0.
    UserIORead,

    /// WAL group-commit fsync + post-write ghost hooks. P1. Never preempted.
    WriterDurable,

    /// Memtable -> L0 SSTable flush. P2.
    Flush,

    /// Compaction reads + writes during k-way merge. P3. The
    /// `target_level` is used at dispatch time to give low-level
    /// compactions (L0->L1, L1->L2) precedence over high-level ones,
    /// per SILK's "low-level critical, high-level background"
    /// principle.
    Compaction { target_level: u8 },

    /// Bulk range scan whose on-disk span EXCEEDS the block-cache
    /// capacity. Same admission behaviour as Flush/Compaction — uses cache
    /// hits but does NOT admit on miss — so a sweep larger than the cache
    /// cannot self-evict and cannot evict the hot working set (it would
    /// thrash either way). A scan that FITS the cache stays on
    /// [`Lane::UserIORead`] and admits exactly as before (zero regression
    /// by design); point lookups never use this lane. The span/capacity
    /// decision is made at bulk-iterator construction. Introduced in 0.9
    /// Fase 1 (G2). Read-only classification — no throttling.
    Scan,
}

impl Lane {
    /// Discriminant for indexing per-lane atomic arrays. All
    /// `Compaction { .. }` variants collapse to the same index;
    /// sub-priority by `target_level` is applied at dispatch time,
    /// not during accounting.
    #[inline]
    pub fn index(self) -> usize {
        match self {
            Lane::UserIORead => 0,
            Lane::WriterDurable => 1,
            Lane::Flush => 2,
            Lane::Compaction { .. } => 3,
            Lane::Scan => 4,
        }
    }

    /// Number of distinct lane indices. Used to size per-lane arrays.
    pub const COUNT: usize = 5;
}

/// Operation taxonomy used by `before_op` / `after_op` to classify the
/// kind of I/O attempted. Granularity is per kernel syscall, not per
/// engine-level abstraction (see shape proposal §9 decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// A `pread`-style read of `bytes` bytes.
    Read { bytes: u32 },
    /// A `write`-style write of `bytes` bytes.
    Write { bytes: u32 },
    /// An `fsync` / `fdatasync`. No size — the cost is the seek + barrier.
    Fsync,
}

/// Per-lane metrics. Captured by [`LanedScheduler`] and surfaced in the
/// `STATS` response under `scheduler.per_lane[*]`. Aggregate by lane,
/// not by tree — trees collapse onto the four lanes (the same anchor
/// lookup on `spatial` and on `identity` both pay UserIORead).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaneMetrics {
    /// Number of `after_op` calls observed for this lane.
    pub ops: u64,
    /// Sum of `elapsed_us` reported across `after_op` calls.
    pub elapsed_us_total: u64,
    /// Operations currently in flight (`before_op` minus `after_op`).
    /// Snapshot may be momentarily negative under concurrent
    /// before/after races; clamped to 0 in [`LaneMetrics::outstanding_clamped`].
    pub outstanding: i32,
    /// Peak observed `outstanding` value since this scheduler was
    /// constructed. Diagnostic only — used by H1 to validate that the
    /// bounded-outstanding ladder kicks in before exceeding the cap.
    pub outstanding_peak: u32,
}

impl LaneMetrics {
    /// Average elapsed microseconds per op. Returns `0.0` when `ops == 0`.
    pub fn avg_elapsed_us(&self) -> f64 {
        if self.ops == 0 {
            0.0
        } else {
            self.elapsed_us_total as f64 / self.ops as f64
        }
    }

    /// `outstanding`, but never negative. The atomic counter can briefly
    /// dip below zero if an `after_op` lands before its paired
    /// `before_op` is observed by a snapshot reader; the clamp hides that
    /// transient artifact.
    pub fn outstanding_clamped(&self) -> u32 {
        self.outstanding.max(0) as u32
    }
}

/// Snapshot of all per-lane metrics. Indexed by [`Lane::index`]:
///
/// - `[0]` UserIORead
/// - `[1]` WriterDurable
/// - `[2]` Flush
/// - `[3]` Compaction
/// - `[4]` Scan
#[derive(Debug, Clone, Copy, Default)]
pub struct SchedulerMetrics {
    pub per_lane: [LaneMetrics; Lane::COUNT],
}

impl SchedulerMetrics {
    /// Convenience accessor that returns the per-lane metrics for `lane`.
    pub fn lane(&self, lane: Lane) -> LaneMetrics {
        self.per_lane[lane.index()]
    }
}

/// Top-level scheduler. **Enum dispatch** — not a trait object — so that
/// the [`Scheduler::Passthrough`] arm compiles to a no-op the optimizer
/// can elide. This is a load-bearing decision for the SSD G5 gate.
// One variant is larger; boxing it would add an indirection to the load-bearing
// zero-overhead Passthrough arm. Boxing is a design change, deferred (not a lint fix).
#[allow(clippy::large_enum_variant)]
pub enum Scheduler {
    /// Zero-overhead pass-through. Default for `StorageProfile::Ssd`. Every
    /// method body is empty for this variant.
    Passthrough,

    /// Lane-aware scheduler. Active for `StorageProfile::Hdd`. Spike A
    /// version is observe-only; H1 adds real throttling.
    Laned(LanedScheduler),
}

impl Scheduler {
    /// Convenience constructor — returns the zero-overhead variant. All
    /// production call sites that have no specific scheduling need
    /// (tests, in-process tools) pass an `Arc::new(Scheduler::passthrough())`.
    pub fn passthrough() -> Self {
        Scheduler::Passthrough
    }

    /// Construct a fresh laned scheduler for per-lane observability:
    /// service-time sliding windows, EWMA P50, outstanding counters,
    /// SLO breach detection, cross-lane peak. Used by `TurbaEngine::open`
    /// when the storage profile selects HDD. v0.5 retired the enforce
    /// ladder per DEC-V5-11; this is pure instrumentation without
    /// throttling.
    pub fn laned() -> Self {
        Scheduler::Laned(LanedScheduler::with_config(
            LanedSchedulerConfig::observe_only(),
        ))
    }

    /// Invoked by call sites BEFORE issuing the underlying I/O. At Spike A
    /// level this is a no-op for both variants; H1 plugs in token-bucket
    /// consume + bounded-outstanding waits.
    #[inline]
    pub fn before_op(&self, lane: Lane, kind: OpKind) {
        match self {
            Scheduler::Passthrough => {}
            Scheduler::Laned(s) => s.before_op(lane, kind),
        }
    }

    /// Invoked by call sites AFTER the underlying I/O completes (success
    /// or failure). `elapsed_us` is the wall-clock time spent on the
    /// attempt, regardless of outcome — analogous to the Day 0-1 cache
    /// counter semantics (see `cache.rs` `fix(engine)` add6505).
    #[inline]
    pub fn after_op(&self, lane: Lane, kind: OpKind, elapsed_us: u64) {
        match self {
            Scheduler::Passthrough => {}
            Scheduler::Laned(s) => s.after_op(lane, kind, elapsed_us),
        }
    }

    /// Snapshot the per-lane metrics. Returns the zero snapshot for
    /// `Passthrough` (all lanes report `ops = 0` because Passthrough
    /// never accumulates).
    pub fn metrics(&self) -> SchedulerMetrics {
        match self {
            Scheduler::Passthrough => SchedulerMetrics::default(),
            Scheduler::Laned(s) => s.metrics(),
        }
    }

    /// Stable identifier for this scheduler variant. Used by STATS
    /// surfacing to expose which scheduler is active to operators.
    pub fn mode_str(&self) -> &'static str {
        match self {
            Scheduler::Passthrough => "passthrough",
            Scheduler::Laned(_) => "laned",
        }
    }

    /// Query the H1.2 sliding-window P50 service time (µs) for `lane`.
    /// Returns `None` for [`Scheduler::Passthrough`] (no windows kept)
    /// or when the laned window is empty for the past 1 s. Has the side
    /// effect of updating the EWMA and SLO breach counter on Laned —
    /// see [`LanedScheduler::p50_us`].
    pub fn p50_us(&self, lane: Lane) -> Option<u64> {
        match self {
            Scheduler::Passthrough => None,
            Scheduler::Laned(s) => s.p50_us(lane),
        }
    }

    /// Read the EWMA P50 service time (µs) for `lane`. Returns 0 for
    /// [`Scheduler::Passthrough`] (no windows kept) or when no
    /// `p50_us(lane)` query has produced a sample yet on Laned.
    pub fn ewma_p50_us(&self, lane: Lane) -> u64 {
        match self {
            Scheduler::Passthrough => 0,
            Scheduler::Laned(s) => s.ewma_p50_us(lane),
        }
    }

    /// Read the cumulative SLO-breach count for `lane`. 0 for
    /// Passthrough; observability metric on Laned. Preserved post-v0.5
    /// ladder retirement (DEC-V5-11) for future SLO-aware policies.
    pub fn slo_breach_count(&self, lane: Lane) -> u64 {
        match self {
            Scheduler::Passthrough => 0,
            Scheduler::Laned(s) => s.slo_breach_count(lane),
        }
    }

    /// Read the cumulative cross-lane outstanding peak — sum of in-flight
    /// ops across all four lanes, peak observed over the scheduler's
    /// lifetime. 0 for Passthrough (no instrumentation). Spike B (v0.3.2)
    /// primary metric for kernel-level disk-queue saturation.
    pub fn cross_lane_outstanding_peak(&self) -> u32 {
        match self {
            Scheduler::Passthrough => 0,
            Scheduler::Laned(s) => s.cross_lane_outstanding_peak(),
        }
    }
}

/// Sliding window of `(timestamp, elapsed_us)` samples for one lane.
/// Backs the service-time P50 estimate. Fixed cap [`SERVICE_TIME_WINDOW_CAP`]; pushes evict samples
/// older than [`SERVICE_TIME_WINDOW_DURATION`] at push time, and a
/// defensive filter at query time drops anything that survived the
/// eviction race (rare under typical lane I/O frequency, but cheap).
///
/// Synchronised by `parking_lot::Mutex`. Lock contention is bounded by
/// per-lane I/O frequency: UserIORead ~100 ops/s on HDD, Compaction
/// peak ≤ 5 in-flight per Spike A.3 STATS — well below the contention
/// threshold for parking_lot.
struct ServiceTimeWindow {
    samples: parking_lot::Mutex<VecDeque<(Instant, u64)>>,
}

/// Maximum samples kept per lane in the sliding window. At HDD UserIORead
/// rates (~100 ops/s) and 1 s window, ~100 samples fits with margin.
pub const SERVICE_TIME_WINDOW_CAP: usize = 128;

/// Sliding-window duration for the P50 service-time estimate.
pub const SERVICE_TIME_WINDOW_DURATION: Duration = Duration::from_secs(1);

impl ServiceTimeWindow {
    fn new() -> Self {
        Self {
            samples: parking_lot::Mutex::new(VecDeque::with_capacity(SERVICE_TIME_WINDOW_CAP)),
        }
    }

    /// Push a new sample. Evicts samples older than
    /// [`SERVICE_TIME_WINDOW_DURATION`] from the front. Caps the deque at
    /// [`SERVICE_TIME_WINDOW_CAP`] by dropping the oldest if full.
    fn push(&self, elapsed_us: u64) {
        let now = Instant::now();
        let cutoff = now - SERVICE_TIME_WINDOW_DURATION;
        let mut q = self.samples.lock();
        while let Some(&(ts, _)) = q.front() {
            if ts < cutoff {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() == SERVICE_TIME_WINDOW_CAP {
            q.pop_front();
        }
        q.push_back((now, elapsed_us));
    }

    /// Compute P50 over samples within the last
    /// [`SERVICE_TIME_WINDOW_DURATION`]. Returns `None` if the window is
    /// empty after filtering. Cost is O(N log N) on the size of the
    /// in-window slice (N ≤ [`SERVICE_TIME_WINDOW_CAP`]); not on the hot
    /// path — query-time only.
    fn p50_us(&self, now: Instant) -> Option<u64> {
        let cutoff = now.checked_sub(SERVICE_TIME_WINDOW_DURATION)?;
        let q = self.samples.lock();
        let mut elapsed: Vec<u64> = q
            .iter()
            .filter(|(ts, _)| *ts >= cutoff)
            .map(|(_, e)| *e)
            .collect();
        if elapsed.is_empty() {
            return None;
        }
        elapsed.sort_unstable();
        Some(elapsed[elapsed.len() / 2])
    }
}

/// Configuration for [`LanedScheduler`]. The scheduler is observability-only:
/// it measures per-lane service time (`slo_us` breach detection) and outstanding
/// peaks (`max_outstanding_per_lane`) but never throttles. The reader-feedback
/// enforce ladder these knobs were meant to drive was evaluated and retired —
/// net-negative under realistic workloads — so the knobs stay observe-only.
#[derive(Debug, Clone, Copy)]
pub struct LanedSchedulerConfig {
    /// Token bucket refill rate (bytes/sec) per lane. `f64::INFINITY` at
    /// Spike A.3 — no throttling. H1 sets to e.g. 40 MB/s for
    /// Compaction on HDD.
    pub rate_bytes_per_sec: [f64; Lane::COUNT],
    /// Token bucket capacity (bytes) per lane. Burst tolerance.
    pub capacity_bytes: [f64; Lane::COUNT],
    /// Soft cap on simultaneous in-flight ops per lane. Exceeding it is
    /// observed (peak counter) but does not block at Spike A.3.
    pub max_outstanding_per_lane: [u32; Lane::COUNT],
    /// Per-lane SLO threshold (µs) for service-time breach detection.
    /// Observe-only: breaches are counted for observability, never enforced
    /// (example targets: UserIORead ~30 ms HDD, WriterDurable ~10 ms HDD).
    pub slo_us: [u64; Lane::COUNT],
}

impl LanedSchedulerConfig {
    /// Default observe-only configuration: every knob set to a value
    /// that disables throttling and breach detection. Bench-A behaviour
    /// with this config must be byte-equivalent to running
    /// [`Scheduler::Passthrough`] within run-to-run noise — the
    /// property the Spike A.3 HDD baseline + H1.2 G5 gate on.
    pub fn observe_only() -> Self {
        Self {
            rate_bytes_per_sec: [f64::INFINITY; Lane::COUNT],
            capacity_bytes: [f64::INFINITY; Lane::COUNT],
            max_outstanding_per_lane: [u32::MAX; Lane::COUNT],
            slo_us: [u64::MAX; Lane::COUNT],
        }
    }
}

impl Default for LanedSchedulerConfig {
    fn default() -> Self {
        Self::observe_only()
    }
}

/// Lane-aware scheduler shell. **Spike A.3 is observe-only**: `before_op`
/// increments outstanding counters but does not block; `after_op`
/// accumulates op count, elapsed-us, and updates the outstanding counter
/// + peak.
///
/// The struct intentionally lives in the same module as [`Scheduler`]
/// at this stage to keep the shell visible alongside the public surface;
/// it will move to its own file when H1 grows it past ~200 LOC.
pub struct LanedScheduler {
    /// Per-lane op counter. Indexed by `Lane::index()`.
    ops: [AtomicU64; Lane::COUNT],
    /// Per-lane accumulator of elapsed time (µs). Indexed identically.
    elapsed_us: [AtomicU64; Lane::COUNT],
    /// Per-lane currently-in-flight op count: incremented by `before_op`,
    /// decremented by `after_op`. Signed so a momentary negative value
    /// caused by snapshot/order races is observable (and clamped via
    /// [`LaneMetrics::outstanding_clamped`]).
    outstanding: [AtomicI32; Lane::COUNT],
    /// Per-lane peak observed `outstanding`. Monotonic; updated CAS-style
    /// inside `before_op`.
    outstanding_peak: [AtomicU32; Lane::COUNT],
    /// Per-lane sliding window of recent service times for the H1.2 P50
    /// estimate. Pushed in `after_op`; queried on-demand via [`p50_us`].
    service_time_windows: [ServiceTimeWindow; Lane::COUNT],
    /// Per-lane EWMA of P50 service time, **in nanoseconds**. Updated
    /// only when `p50_us(lane)` is queried with a populated window.
    /// α = 0.3. 0 if no query has produced a P50 yet.
    ///
    /// Stored in ns rather than µs so the integer EWMA arithmetic
    /// `(3·new + 7·prev) / 10` preserves sub-µs precision across
    /// iterations. With µs storage, e.g. p50=1µs after prev=2µs
    /// degrades to 1µs (loses 0.7µs); with ns storage the same scenario
    /// tracks at 1700 ns (truthful). The public accessor
    /// [`ewma_p50_us`] floors the value to whole µs for STATS schema
    /// stability.
    ewma_p50_ns: [AtomicU64; Lane::COUNT],
    /// Per-lane count of `current_p50 > slo_us` events observed at query
    /// time. Preserved post-v0.5 ladder retirement (DEC-V5-11) for future
    /// SLO-aware policies; defaults to 0 with `slo_us = u64::MAX`.
    slo_breach_count: [AtomicU64; Lane::COUNT],
    /// Cross-lane sum of currently-in-flight ops (signed for the same
    /// race-tolerance reason as the per-lane counters). Incremented by
    /// every `before_op` regardless of lane; decremented by every
    /// `after_op`. Together with `cross_lane_outstanding_peak` this
    /// surfaces kernel-level disk-queue saturation (kernel sees all
    /// lanes' ops simultaneously, while per-lane peaks may stagger in
    /// time). Added in v0.3.2 Spike B.
    cross_lane_outstanding: AtomicI32,
    /// Monotonic peak of `cross_lane_outstanding`. CAS-updated inside
    /// `before_op` after the increment. Cumulative over the scheduler's
    /// lifetime (no reset capability — Spike B uses pre/post-phase
    /// snapshot deltas plus the histogram + miss-count deltas to derive
    /// phase-local I/O dynamics; see Spike B doc §2.2).
    cross_lane_outstanding_peak: AtomicU32,
    config: LanedSchedulerConfig,
}

impl LanedScheduler {
    pub fn new() -> Self {
        Self::with_config(LanedSchedulerConfig::observe_only())
    }

    pub fn with_config(config: LanedSchedulerConfig) -> Self {
        Self {
            ops: [const { AtomicU64::new(0) }; Lane::COUNT],
            elapsed_us: [const { AtomicU64::new(0) }; Lane::COUNT],
            outstanding: [const { AtomicI32::new(0) }; Lane::COUNT],
            outstanding_peak: [const { AtomicU32::new(0) }; Lane::COUNT],
            service_time_windows: [
                ServiceTimeWindow::new(),
                ServiceTimeWindow::new(),
                ServiceTimeWindow::new(),
                ServiceTimeWindow::new(),
                ServiceTimeWindow::new(),
            ],
            ewma_p50_ns: [const { AtomicU64::new(0) }; Lane::COUNT],
            slo_breach_count: [const { AtomicU64::new(0) }; Lane::COUNT],
            cross_lane_outstanding: AtomicI32::new(0),
            cross_lane_outstanding_peak: AtomicU32::new(0),
            config,
        }
    }

    /// Read the cumulative cross-lane outstanding peak. Sums all four
    /// lanes' `before_op` invocations minus their `after_op` matches,
    /// peak observed since scheduler construction. 0 on Passthrough.
    /// Spike B (v0.3.2) primary metric for kernel-level disk-queue
    /// saturation.
    pub fn cross_lane_outstanding_peak(&self) -> u32 {
        self.cross_lane_outstanding_peak.load(Ordering::Relaxed)
    }

    /// Increment per-lane and cross-lane outstanding counters. v0.5
    /// retired the enforce ladder per DEC-V5-11; `before_op` is now pure
    /// instrumentation (no blocking, no throttling) for all lanes.
    #[inline]
    pub fn before_op(&self, lane: Lane, _kind: OpKind) {
        let idx = lane.index();
        let new_outstanding = self.outstanding[idx].fetch_add(1, Ordering::Relaxed) + 1;
        if new_outstanding > 0 {
            // CAS-update the peak. Lossy under contention but acceptable
            // for a diagnostic counter — at worst we under-count peak.
            let unsigned = new_outstanding as u32;
            let _ = self.outstanding_peak[idx].fetch_max(unsigned, Ordering::Relaxed);
        }
        // Cross-lane outstanding tracking (Spike B v0.3.2). One additional
        // fetch_add + fetch_max per before_op; both Relaxed (lossy peak
        // under contention is acceptable for a diagnostic counter, same
        // semantics as per-lane peak above).
        let cross = self.cross_lane_outstanding.fetch_add(1, Ordering::Relaxed) + 1;
        if cross > 0 {
            let _ = self
                .cross_lane_outstanding_peak
                .fetch_max(cross as u32, Ordering::Relaxed);
        }
    }

    /// Decrement outstanding + accumulate op count and elapsed time, and
    /// push the elapsed sample onto the per-lane sliding window for the
    /// H1.2 P50 estimate. `Ordering::Relaxed` is sufficient — no
    /// cross-counter invariant is enforced and consumers read each field
    /// independently.
    #[inline]
    pub fn after_op(&self, lane: Lane, _kind: OpKind, elapsed_us: u64) {
        let idx = lane.index();
        self.outstanding[idx].fetch_sub(1, Ordering::Relaxed);
        self.ops[idx].fetch_add(1, Ordering::Relaxed);
        self.elapsed_us[idx].fetch_add(elapsed_us, Ordering::Relaxed);
        self.service_time_windows[idx].push(elapsed_us);
        // Cross-lane outstanding tracking (Spike B v0.3.2): one additional
        // fetch_sub per after_op.
        self.cross_lane_outstanding.fetch_sub(1, Ordering::Relaxed);
    }

    /// Compute the current P50 service time over the last 1 s for `lane`
    /// and update the EWMA + SLO breach counter as a side effect. Returns
    /// `None` if the window is empty (no I/O on this lane in the past 1 s).
    /// Cost is O(N log N) per call (N ≤ 128); not on the hot path.
    pub fn p50_us(&self, lane: Lane) -> Option<u64> {
        let idx = lane.index();
        let p50 = self.service_time_windows[idx].p50_us(Instant::now())?;
        // Update EWMA: ewma = α * new + (1 - α) * prev, α = 0.3.
        // Computed in ns to preserve sub-µs precision across iterations
        // (see `ewma_p50_ns` field comment). saturating_* guards against
        // overflow on absurd `elapsed_us` values.
        let p50_ns = p50.saturating_mul(1000);
        let prev_ns = self.ewma_p50_ns[idx].load(Ordering::Relaxed);
        let next_ns = if prev_ns == 0 {
            p50_ns
        } else {
            // (3·new + 7·prev) / 10  — integer arithmetic version of
            // 0.3·new + 0.7·prev.
            let weighted = p50_ns
                .saturating_mul(3)
                .saturating_add(prev_ns.saturating_mul(7));
            weighted / 10
        };
        self.ewma_p50_ns[idx].store(next_ns, Ordering::Relaxed);
        if p50 > self.config.slo_us[idx] {
            self.slo_breach_count[idx].fetch_add(1, Ordering::Relaxed);
        }
        Some(p50)
    }

    /// Read the EWMA P50 service time (µs) for `lane`. 0 if no
    /// `p50_us(lane)` query has produced a sample yet. Floors the
    /// internal ns-scaled value to whole µs for STATS schema stability.
    pub fn ewma_p50_us(&self, lane: Lane) -> u64 {
        self.ewma_p50_ns[lane.index()].load(Ordering::Relaxed) / 1000
    }

    /// Read the cumulative SLO-breach count for `lane`. Stays at 0 under
    /// the default `LanedSchedulerConfig::observe_only` configuration
    /// because `slo_us` is `u64::MAX`. H1.3 supplies real values.
    pub fn slo_breach_count(&self, lane: Lane) -> u64 {
        self.slo_breach_count[lane.index()].load(Ordering::Relaxed)
    }

    /// Snapshot all four lanes' counters. Cheap (16 atomic loads).
    pub fn metrics(&self) -> SchedulerMetrics {
        let mut per_lane = [LaneMetrics::default(); Lane::COUNT];
        for (i, slot) in per_lane.iter_mut().enumerate() {
            *slot = LaneMetrics {
                ops: self.ops[i].load(Ordering::Relaxed),
                elapsed_us_total: self.elapsed_us[i].load(Ordering::Relaxed),
                outstanding: self.outstanding[i].load(Ordering::Relaxed),
                outstanding_peak: self.outstanding_peak[i].load(Ordering::Relaxed),
            };
        }
        SchedulerMetrics { per_lane }
    }
}

impl Default for LanedScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn lane_index_is_stable_per_variant() {
        assert_eq!(Lane::UserIORead.index(), 0);
        assert_eq!(Lane::WriterDurable.index(), 1);
        assert_eq!(Lane::Flush.index(), 2);
        assert_eq!(Lane::Compaction { target_level: 0 }.index(), 3);
        assert_eq!(Lane::Compaction { target_level: 5 }.index(), 3);
        // All Compaction { .. } collapse onto index 3 by design.
        assert_eq!(Lane::Scan.index(), 4);
    }

    #[test]
    fn lane_count_matches_variants() {
        assert_eq!(Lane::COUNT, 5);
    }

    #[test]
    fn passthrough_metrics_are_zero() {
        let s = Scheduler::passthrough();
        s.before_op(Lane::UserIORead, OpKind::Read { bytes: 64 });
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 1234);
        s.before_op(
            Lane::Compaction { target_level: 2 },
            OpKind::Write { bytes: 1024 },
        );
        s.after_op(
            Lane::Compaction { target_level: 2 },
            OpKind::Write { bytes: 1024 },
            5000,
        );

        let m = s.metrics();
        for i in 0..Lane::COUNT {
            assert_eq!(
                m.per_lane[i].ops, 0,
                "Passthrough must not accumulate (lane {i})"
            );
            assert_eq!(m.per_lane[i].elapsed_us_total, 0);
        }
    }

    #[test]
    fn laned_after_op_accumulates_on_correct_lane() {
        let s = Scheduler::laned();
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 100);
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 200);
        s.after_op(Lane::Flush, OpKind::Write { bytes: 1024 }, 500);
        s.after_op(
            Lane::Compaction { target_level: 1 },
            OpKind::Write { bytes: 4096 },
            800,
        );
        s.after_op(
            Lane::Compaction { target_level: 5 },
            OpKind::Read { bytes: 4096 },
            700,
        );

        let m = s.metrics();
        assert_eq!(m.lane(Lane::UserIORead).ops, 2);
        assert_eq!(m.lane(Lane::UserIORead).elapsed_us_total, 300);
        assert_eq!(m.lane(Lane::WriterDurable).ops, 0);
        assert_eq!(m.lane(Lane::Flush).ops, 1);
        assert_eq!(m.lane(Lane::Flush).elapsed_us_total, 500);
        // Both compactions collapse onto the same slot:
        assert_eq!(m.lane(Lane::Compaction { target_level: 0 }).ops, 2);
        assert_eq!(
            m.lane(Lane::Compaction { target_level: 0 })
                .elapsed_us_total,
            1500
        );
    }

    #[test]
    fn laned_before_op_does_not_count_ops_or_elapsed_us() {
        // Spike A.3 semantics: before_op tracks outstanding but does NOT
        // count toward ops/elapsed_us totals (those are after_op's job).
        // H1 may add throttling logic to before_op but the ops/elapsed_us
        // contract stays.
        let s = Scheduler::laned();
        for _ in 0..1000 {
            s.before_op(Lane::UserIORead, OpKind::Read { bytes: 64 });
        }
        let m = s.metrics();
        assert_eq!(m.lane(Lane::UserIORead).ops, 0);
        assert_eq!(m.lane(Lane::UserIORead).elapsed_us_total, 0);
        // outstanding should track the 1000 unmatched before_op calls
        assert_eq!(m.lane(Lane::UserIORead).outstanding, 1000);
        assert_eq!(m.lane(Lane::UserIORead).outstanding_peak, 1000);
    }

    #[test]
    fn outstanding_counter_balances_with_paired_before_after() {
        let s = Scheduler::laned();
        // 100 paired before/after calls — outstanding should land at 0
        for i in 0..100 {
            s.before_op(Lane::UserIORead, OpKind::Read { bytes: 64 });
            // Also ensure peak grows monotonically (each before_op pushes)
            let m = s.metrics();
            assert_eq!(m.lane(Lane::UserIORead).outstanding, 1);
            assert!(m.lane(Lane::UserIORead).outstanding_peak >= 1, "iter {i}");
            s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 1);
        }
        let m = s.metrics();
        assert_eq!(m.lane(Lane::UserIORead).outstanding, 0);
        assert_eq!(m.lane(Lane::UserIORead).outstanding_peak, 1);
        assert_eq!(m.lane(Lane::UserIORead).ops, 100);
    }

    #[test]
    fn outstanding_peak_captures_concurrent_max() {
        // 4 threads × 250 before_ops, no after_op until all done. Peak
        // should land at 1000.
        let s = std::sync::Arc::new(Scheduler::Laned(LanedScheduler::with_config(
            LanedSchedulerConfig::observe_only(),
        )));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s_c = std::sync::Arc::clone(&s);
            handles.push(thread::spawn(move || {
                for _ in 0..250 {
                    s_c.before_op(
                        Lane::Compaction { target_level: 1 },
                        OpKind::Write { bytes: 1024 },
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let m = s.metrics();
        let lane_compaction = m.lane(Lane::Compaction { target_level: 0 });
        assert_eq!(lane_compaction.outstanding, 1000);
        assert_eq!(lane_compaction.outstanding_peak, 1000);
    }

    #[test]
    fn observe_only_config_disables_throttling() {
        // Sanity: the default config IS observe_only.
        let cfg = LanedSchedulerConfig::default();
        for i in 0..Lane::COUNT {
            assert!(cfg.rate_bytes_per_sec[i].is_infinite());
            assert!(cfg.capacity_bytes[i].is_infinite());
            assert_eq!(cfg.max_outstanding_per_lane[i], u32::MAX);
        }
    }

    #[test]
    fn outstanding_clamped_handles_negative() {
        let lm = LaneMetrics {
            outstanding: -3,
            ..Default::default()
        };
        assert_eq!(lm.outstanding_clamped(), 0);
        let lm = LaneMetrics {
            outstanding: 7,
            ..Default::default()
        };
        assert_eq!(lm.outstanding_clamped(), 7);
    }

    #[test]
    fn lane_metrics_avg_handles_zero_ops() {
        let lm = LaneMetrics::default();
        assert_eq!(lm.avg_elapsed_us(), 0.0);
    }

    #[test]
    fn lane_metrics_avg_computes() {
        let lm = LaneMetrics {
            ops: 4,
            elapsed_us_total: 100,
            ..Default::default()
        };
        assert_eq!(lm.avg_elapsed_us(), 25.0);
    }

    #[test]
    fn laned_is_thread_safe_under_concurrent_accumulation() {
        // 4 threads × 250 ops on UserIORead should sum to exactly 1000 ops
        // and 1000 * 7 = 7000 elapsed_us. Validates that Relaxed atomics
        // don't lose updates under contention (which would be a real bug).
        let s = Arc::new(Scheduler::laned());
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s_c = Arc::clone(&s);
            handles.push(thread::spawn(move || {
                for _ in 0..250 {
                    s_c.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 7);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let m = s.metrics();
        assert_eq!(m.lane(Lane::UserIORead).ops, 1000);
        assert_eq!(m.lane(Lane::UserIORead).elapsed_us_total, 7000);
    }

    // --- H1.2 service-time sliding window + EWMA P50 ---

    #[test]
    fn service_time_window_returns_none_when_empty() {
        let w = ServiceTimeWindow::new();
        assert_eq!(w.p50_us(Instant::now()), None);
    }

    #[test]
    fn service_time_window_returns_p50_after_pushes() {
        let w = ServiceTimeWindow::new();
        // Push 7 values; sorted [10, 20, 30, 40, 50, 60, 70], median index 3 → 40.
        for v in [40u64, 10, 70, 30, 60, 20, 50] {
            w.push(v);
        }
        assert_eq!(w.p50_us(Instant::now()), Some(40));
    }

    #[test]
    fn service_time_window_evicts_samples_older_than_1s() {
        // We can't fast-forward Instant::now() in std, so we exploit the
        // query-time defensive filter: query with a future `now` so the
        // cutoff falls beyond any pushed sample's timestamp.
        let w = ServiceTimeWindow::new();
        for v in [10u64, 20, 30, 40, 50] {
            w.push(v);
        }
        assert_eq!(w.p50_us(Instant::now()), Some(30));
        // 2 s in the future → cutoff is 1 s in the future → all real
        // samples drop out of the window.
        let future = Instant::now() + Duration::from_secs(2);
        assert_eq!(w.p50_us(future), None);
    }

    #[test]
    fn laned_scheduler_p50_per_lane_isolated() {
        let s = Scheduler::laned();
        // Push samples on UserIORead only; Compaction stays empty.
        for v in [100u64, 200, 300] {
            s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, v);
        }
        assert_eq!(s.p50_us(Lane::UserIORead), Some(200));
        assert_eq!(s.p50_us(Lane::Compaction { target_level: 0 }), None);
        assert_eq!(s.p50_us(Lane::Flush), None);
        assert_eq!(s.p50_us(Lane::WriterDurable), None);
    }

    #[test]
    fn laned_scheduler_ewma_updates_on_query() {
        let s = Scheduler::laned();
        // First sample triggers initialisation (ewma = first p50).
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 100);
        assert_eq!(s.p50_us(Lane::UserIORead), Some(100));
        assert_eq!(s.ewma_p50_us(Lane::UserIORead), 100);

        // Push a second batch; integer EWMA = (3 * 200 + 7 * 100) / 10 = 130.
        for v in [200u64, 200, 200] {
            s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, v);
        }
        assert_eq!(s.p50_us(Lane::UserIORead), Some(200));
        assert_eq!(s.ewma_p50_us(Lane::UserIORead), 130);
    }

    #[test]
    fn laned_scheduler_slo_breach_observe_only_default() {
        // Observe-only config has slo_us = u64::MAX per lane → no breach
        // can fire under any sample rate.
        let s = Scheduler::Laned(LanedScheduler::with_config(
            LanedSchedulerConfig::observe_only(),
        ));
        for v in [1_000_000u64; 5] {
            s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, v);
        }
        // Drive the EWMA + breach detector via the query path.
        let _ = s.p50_us(Lane::UserIORead);
        assert_eq!(
            s.slo_breach_count(Lane::UserIORead),
            0,
            "observe-only default must not trip SLO breach counter"
        );
    }

    #[test]
    fn passthrough_p50_is_none_and_ewma_is_zero() {
        let s = Scheduler::passthrough();
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 1234);
        assert_eq!(s.p50_us(Lane::UserIORead), None);
        assert_eq!(s.ewma_p50_us(Lane::UserIORead), 0);
        assert_eq!(s.slo_breach_count(Lane::UserIORead), 0);
    }

    // --- EWMA ns-scaled precision (regression guard) ---

    /// Helper: build a Laned scheduler with a tight SLO so EWMA tests
    /// can drive deterministic transitions on synthetic µs-level samples.
    fn laned_with_tight_slo(slo_us: u64, _max_outstanding: u32) -> LanedScheduler {
        let mut cfg = LanedSchedulerConfig::observe_only();
        cfg.slo_us[Lane::UserIORead.index()] = slo_us;
        LanedScheduler::with_config(cfg)
    }

    #[test]
    fn ewma_internal_storage_is_ns_scaled() {
        // After one 100 µs sample, internal EWMA storage should hold
        // 100 000 ns; the public accessor should still floor to 100 µs.
        let s = laned_with_tight_slo(10_000, 8);
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 100);
        let _ = s.p50_us(Lane::UserIORead);
        let internal_ns = s.ewma_p50_ns[Lane::UserIORead.index()].load(Ordering::Relaxed);
        assert_eq!(internal_ns, 100_000, "internal storage in ns");
        assert_eq!(
            s.ewma_p50_us(Lane::UserIORead),
            100,
            "accessor floors to µs"
        );
    }

    #[test]
    fn ewma_preserves_subus_precision_across_iterations() {
        // Drive a 1 µs sample. With µs-only arithmetic the subsequent
        // 2 µs sample collapses the EWMA to 1 (loses 0.7 µs of signal).
        // With ns-scaled storage we observe the precise 1700 ns
        // intermediate that round-trips correctly through later
        // iterations. Defends the H1.3 design review note flagged in
        // the H1.2 commit body.
        let s = laned_with_tight_slo(10_000, 8);
        // Pin the window to a single 1 µs sample.
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 1);
        let _ = s.p50_us(Lane::UserIORead);
        let after_first_ns = s.ewma_p50_ns[Lane::UserIORead.index()].load(Ordering::Relaxed);
        assert_eq!(after_first_ns, 1_000);

        // A second 2 µs sample. Window now [1, 2]; median (q.len()/2 = 1)
        // returns the higher value 2. EWMA in ns:
        // (3·2000 + 7·1000) / 10 = 1300.
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 2);
        let _ = s.p50_us(Lane::UserIORead);
        let after_second_ns = s.ewma_p50_ns[Lane::UserIORead.index()].load(Ordering::Relaxed);
        assert_eq!(
            after_second_ns, 1_300,
            "ns-scaled EWMA preserves the 1.3 µs intermediate; µs-only \
             arithmetic would have collapsed to 1 (= (3·2 + 7·1) / 10)"
        );
        // Accessor floors to 1 µs because that's what STATS exposes.
        assert_eq!(s.ewma_p50_us(Lane::UserIORead), 1);
    }

    #[test]
    fn cross_lane_outstanding_peak_tracks_simultaneous_ops_across_lanes() {
        // Spike B (v0.3.2) regression: cross_lane_outstanding_peak must
        // reflect the simultaneous in-flight count across ALL lanes, not
        // a per-lane peak. Sum-of-per-lane-peaks ≠ peak-of-sum when peaks
        // stagger in time. This test pins the simultaneous-sum semantics.
        let s = LanedScheduler::new();

        // Stage three lanes simultaneously: 2 UserIORead + 3 Compaction
        // + 1 Flush in flight. Per-lane peaks: 2, 3, 1. Cross-lane peak: 6.
        for _ in 0..2 {
            s.before_op(Lane::UserIORead, OpKind::Read { bytes: 64 });
        }
        for _ in 0..3 {
            s.before_op(
                Lane::Compaction { target_level: 0 },
                OpKind::Read { bytes: 64 },
            );
        }
        s.before_op(Lane::Flush, OpKind::Write { bytes: 1024 });

        assert_eq!(
            s.cross_lane_outstanding_peak(),
            6,
            "cross-lane peak must equal simultaneous total across lanes"
        );

        // Drain back to 0; peak is monotonic so it stays at 6 even after
        // all after_ops decrement the running count.
        for _ in 0..2 {
            s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 1);
        }
        for _ in 0..3 {
            s.after_op(
                Lane::Compaction { target_level: 0 },
                OpKind::Read { bytes: 64 },
                1,
            );
        }
        s.after_op(Lane::Flush, OpKind::Write { bytes: 1024 }, 1);

        assert_eq!(
            s.cross_lane_outstanding_peak(),
            6,
            "peak is monotonic; remains 6 after lanes drain"
        );
    }

    #[test]
    fn passthrough_cross_lane_outstanding_peak_is_zero() {
        // Passthrough is unconditionally 0 — no instrumentation.
        let s = Scheduler::passthrough();
        s.before_op(Lane::UserIORead, OpKind::Read { bytes: 64 });
        s.after_op(Lane::UserIORead, OpKind::Read { bytes: 64 }, 100);
        assert_eq!(s.cross_lane_outstanding_peak(), 0);
    }
}
