//! Bounded pool for ephemeral ghost-creation work — the
//! `v0.3.2-ghost-singleflight` cycle's load-bearing intervention.
//!
//! Spike D measured 74.4 % of 8R CPU samples in
//! `Engine::maybe_create_ephemeral_ghost`'s spawned closure (vs 0 % at
//! 1R). PASO 3 quantified the per-event count (4-10 spawns over 5 min ×
//! 8R) and falsified the dedup-race-as-amplifier framing
//! (`dedup_lost = 0` over two runs). The amplifier is therefore
//! *parallelism × per-spawn cost*: each spawn does a full lobe scan +
//! per-record deserialize (~75-200 s of CPU on Scale 1.0 hot regime),
//! and the pre-fix code spawns one OS thread per candidate without
//! coordination so several can run concurrently and saturate the read
//! path's CPU budget.
//!
//! This pool serializes ghost-creation work into `N = clamp(cpus / 2,
//! 1, 4)` worker threads with a small bounded backlog. When the
//! backlog is full, new candidates are dropped (the read path's
//! caller increments `ghost_pool_dropped_full_count`); ghost
//! creation is opportunistic, so a dropped candidate will refire if
//! traffic is sustained and telemetry will re-detect.
//!
//! Lifecycle is tied to the engine: pool is created in `Engine::open`,
//! workers exit cleanly on engine drop (sender drops → receivers see
//! `Disconnected` → loop exits → `JoinHandle::join`).

// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use dashmap::DashSet;

use crate::engine::Engine;
use crate::scan_telemetry::AutoGhostCandidate;

/// RAII guard for the single-flight in-flight set.
///
/// When `Engine::maybe_create_ephemeral_ghost` decides to dispatch a
/// candidate, it inserts the candidate's `filter_desc` hash into
/// `Engine::ghost_inflight` and constructs a guard for that hash.
/// The guard is **moved into the `GhostCreateJob`**, so its lifetime
/// spans the synchronous submit, the time on the channel, and the
/// worker's `execute_ghost_job` invocation. The guard's `Drop` removes
/// the hash from the set — running on:
///
/// 1. normal worker completion (worker finishes, Job goes out of
///    scope, guard drops),
/// 2. submit failure (`TrySendError::Full(job)` returns the Job, the
///    wildcard pattern in `submit` drops it, guard drops),
/// 3. worker panic (panic unwinds through the Job's stack frame,
///    guard drops as part of unwind).
///
/// All three paths converge on "hash removed exactly once". A second
/// candidate with the same `filter_desc` arriving at any point can
/// re-enter once any of the three paths fires.
pub(crate) struct SingleflightGuard {
    inflight: Arc<DashSet<u64>>,
    hash: u64,
}

impl SingleflightGuard {
    pub(crate) fn new(inflight: Arc<DashSet<u64>>, hash: u64) -> Self {
        Self { inflight, hash }
    }
}

impl Drop for SingleflightGuard {
    fn drop(&mut self) {
        self.inflight.remove(&self.hash);
    }
}

/// Work item submitted to the ghost-creator pool.
///
/// `engine_arc` keeps the engine alive for the duration the job is in
/// flight (in the channel + processing on a worker). Once the worker
/// finishes processing, the `Arc` drops and the engine's refcount
/// returns to its pre-submit value.
///
/// `lobe_id` and `name` are resolved at submit time
/// (`Engine::maybe_create_ephemeral_ghost`) and threaded through the
/// job to preserve the pre-PASO-6.2 fail-fast-on-unknown-lobe
/// semantics — a candidate for an unknown lobe early-returns without
/// counting against `candidate_spawn_count`.
///
/// `_singleflight_guard` is owned by the Job for its full lifetime
/// (channel + worker frame); see `SingleflightGuard` docstring for
/// the panic-safe RAII contract. Underscore prefix signals
/// "intentionally unread field, present for Drop side-effects".
pub(crate) struct GhostCreateJob {
    pub(crate) candidate: AutoGhostCandidate,
    pub(crate) engine_arc: Arc<Engine>,
    pub(crate) lobe_id: u16,
    pub(crate) name: String,
    pub(crate) _singleflight_guard: SingleflightGuard,
}

/// Bounded pool of ghost-creator worker threads.
///
/// The pool owns the `Sender<GhostCreateJob>`; receivers are cloned to
/// each worker. On `Drop`, the sender is taken out (signalling
/// `Disconnected` to all receivers) and the workers are joined.
pub(crate) struct GhostCreatorPool {
    sender: Option<Sender<GhostCreateJob>>,
    workers: Vec<JoinHandle<()>>,
}

impl GhostCreatorPool {
    /// Create a pool with `n_workers` threads (clamped to a minimum of
    /// 1) and a bounded backlog of `n_workers * 4` slots — small
    /// absorption window for jitter without unbounded queue growth.
    pub(crate) fn new(n_workers: usize) -> Self {
        let n = n_workers.max(1);
        let (tx, rx) = bounded(n * 4);
        let mut workers = Vec::with_capacity(n);
        for i in 0..n {
            let rx_worker = rx.clone();
            let handle = std::thread::Builder::new()
                .name(format!("auto-ghost-worker:{i}"))
                .spawn(move || worker_loop(rx_worker))
                .expect("failed to spawn auto-ghost worker");
            workers.push(handle);
        }
        Self {
            sender: Some(tx),
            workers,
        }
    }

    /// Default sizing per the design doc: `clamp(cpus / 2, 1, 4)`.
    /// Falls back to 1 when `available_parallelism` is unavailable.
    pub(crate) fn default_size() -> usize {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        (cpus / 2).clamp(1, 4)
    }

    /// Submit a job to the pool. Returns `true` if the job was
    /// accepted, `false` if the backlog is full or the pool's sender
    /// has already been closed (engine drop in flight). Never blocks.
    /// Caller increments `ghost_pool_submit_failed_count` on `false`.
    pub(crate) fn submit(&self, job: GhostCreateJob) -> bool {
        let tx = match self.sender.as_ref() {
            Some(s) => s,
            None => return false,
        };
        match tx.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Number of worker threads currently registered. Test-only.
    #[cfg(test)]
    pub(crate) fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Backlog capacity (number of jobs the channel can hold without
    /// dropping). Test-only.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.sender.as_ref().and_then(|s| s.capacity()).unwrap_or(0)
    }
}

impl Drop for GhostCreatorPool {
    fn drop(&mut self) {
        // Drop the sender so all receivers see `Disconnected` and exit
        // their `recv` loop cleanly.
        drop(self.sender.take());
        // Join workers to ensure they finish before this Drop returns.
        // If a worker is mid-job, we wait — the in-flight job holds an
        // `Arc<Engine>` so the engine cannot drop while a job is being
        // processed; the chain unrolls naturally once the job
        // completes.
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(rx: Receiver<GhostCreateJob>) {
    while let Ok(job) = rx.recv() {
        execute_ghost_job(job);
    }
}

/// Worker-side execution of a ghost-creation job.
///
/// Body moved verbatim from `Engine::maybe_create_ephemeral_ghost`'s
/// pre-PASO-6.2 `std::thread::Builder::spawn` closure: the same
/// `enforce_ghost_type_limit` step, the same `field_registry` read
/// guard, the same `ghost_manager.create` call with the same
/// arguments, the same `Ok / Err(GhostExists) / Err(_)` match arm
/// counter increments. The only behavioural change is *where* the
/// work runs: instead of one OS thread per candidate, all candidates
/// are serialised through `N = clamp(cpus / 2, 1, 4)` shared workers.
///
/// `lobe_id` and `name` are pre-resolved by the caller and stored on
/// the job so the worker doesn't repeat the lookup; if the lobe was
/// dropped between submit and worker pickup the downstream
/// `field_registry::get_dict(lobe_id)` returns the stale entry's
/// state — same edge case as the pre-PASO-6.2 spawn closure, which
/// also captured `lobe_id` by value before the spawn.
fn execute_ghost_job(job: GhostCreateJob) {
    let GhostCreateJob {
        candidate,
        engine_arc,
        lobe_id,
        name,
        _singleflight_guard,
    } = job;
    // `_singleflight_guard` stays alive for the entire body via the
    // local binding; it only drops when this function returns,
    // signalling "single-flight slot freed" to subsequent candidates.

    // Count/Sum specs for every field telemetry ever saw in an
    // AGGREGATE pipeline for this pattern. Empty vec → filter-only
    // ghost (no PreComputed short-circuit; router pass 1a skips
    // because has_aggregates is false).
    let aggregate_specs = if candidate.aggregate_fields.is_empty() {
        vec![]
    } else {
        use crate::aggregate_state::{AggOp, COUNT_LABEL, Metric};
        use xytalk_parser::ast::AggregateFunc;
        // A fieldless count() (rides the group total) plus a sum per observed
        // field — the metrics the router's count()/sum(field) short-circuit
        // needs. Labels via the single canonical scheme. No per-metric filters.
        let mut metrics = vec![Metric::new(
            String::new(),
            AggOp::Count,
            COUNT_LABEL.to_string(),
            None,
        )];
        for f in &candidate.aggregate_fields {
            let label = crate::ops::aggregate::canonical_label(&AggregateFunc::Sum(f.clone()));
            metrics.push(Metric::new(f.clone(), AggOp::Sum, label, None));
        }
        metrics
    };

    let has_aggregates = !aggregate_specs.is_empty();

    // Hold the field_registry read-lock only for the duration of
    // create(). FieldDict is not Clone, so we can't copy it out of
    // the guard; instead, we keep the guard alive on this thread for
    // the whole create call. The guard is Send because the RwLock is
    // parking_lot::RwLock — its guards are Send.
    //
    // Max Ephemeral ghosts per lobe = 20 (v0.2.1 empirical default;
    // see `engine.rs` predecessor comment for the SSD Zipf-1.5
    // 1 h benchmark that justified the bump from 10 → 20).
    engine_arc.enforce_ghost_type_limit(
        lobe_id,
        &candidate.lobe,
        crate::ghost::GhostType::Ephemeral,
        20,
    );

    let fr_guard = engine_arc.field_registry.read();
    let field_dict = fr_guard.get_dict(lobe_id);

    let create_result = engine_arc.ghost_manager.create(
        &engine_arc.ghost_spatial_tree(),
        &engine_arc.turba.dictionary,
        lobe_id,
        &name,
        &candidate.lobe,
        // Auto-ghost candidates are always flat-AND (the pool skips OR patterns
        // via the empty-filters gate), so wrapping the Vec as a FilterExpr is
        // lossless.
        xytalk_parser::ast::FilterExpr::from_filters(candidate.filters.clone()),
        "", // no ORDER BY for ephemerals — promotion adds it
        false,
        true, // is_auto
        aggregate_specs,
        vec![], // no GROUP BY
        vec![], // no EMBED
        field_dict,
        None, // no metric-order for ephemerals (v2: auto-promotion)
    );
    drop(fr_guard);

    match create_result {
        Ok(_) => {
            // Reclassify + persist. The TTL reaper will read
            // `ghost_type == Ephemeral` and `ttl_seconds` to evict.
            if let Err(e) = engine_arc.ghost_manager.reclassify_lifecycle(
                &name,
                crate::ghost::GhostType::Ephemeral,
                Some(86_400),
                &engine_arc.turba.dictionary,
            ) {
                tracing::warn!("auto-ghost: reclassify failed for '{}': {e}", name);
            }

            // Register in router with operator-aware filter tuple
            // and the filter_desc string (the latter lets future
            // OR-shaped candidates route via filter_desc equality).
            let filter_fields: Vec<_> = candidate
                .filters
                .iter()
                .map(|f| {
                    (
                        f.field.clone(),
                        crate::ops::convert_filter_op(&f.op),
                        crate::ops::literal_to_value(&f.value),
                    )
                })
                .collect();

            {
                let mut routers = engine_arc.ghost_routers.write();
                let router = routers.entry(lobe_id).or_default();
                router.register_ghost(
                    &name,
                    filter_fields,
                    String::new(),
                    false,
                    has_aggregates,
                    vec![],
                );
                router.set_filter(
                    &name,
                    xytalk_parser::ast::FilterExpr::from_filters(candidate.filters.clone()),
                );
                // (B) pattern identity — read on eviction to clear the telemetry
                // flag so the pattern can re-trigger. NOT used for routing.
                router.set_filter_desc(&name, candidate.filter_desc.clone());
            }

            tracing::info!(
                "auto-ghost created: {} (lobe={}, filter_desc={:?}, has_aggregates={})",
                name,
                candidate.lobe,
                candidate.filter_desc,
                has_aggregates
            );
        }
        Err(xyzdb_core::error::XyzError::GhostExists(_)) => {
            // Lost the dedup race: another worker (or a previous
            // submission of the same filter_desc) already built or is
            // building a ghost with this name. The current worker has
            // already paid the scan + partial build cost — that's the
            // work the v0.3.2 single-flight layer aims to eliminate by
            // short-circuiting at the entry, before submit.
            engine_arc
                .ghost_dedup_lost_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!("auto-ghost lost dedup race for '{name}'");
        }
        Err(e) => {
            // Non-dedup failure: the worker paid the scan + partial
            // build but ghost did not register (Storage error,
            // dictionary contention, bloom rebuild race, etc).
            // Counted separately from dedup_lost — these are NOT what
            // single-flight catches; they need orthogonal handling.
            engine_arc
                .ghost_create_failed_other_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!("auto-ghost create failed for '{name}': {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_creates_n_workers() {
        let pool = GhostCreatorPool::new(2);
        assert_eq!(pool.worker_count(), 2);
        assert_eq!(pool.capacity(), 8); // n * 4
    }

    #[test]
    fn pool_clamps_zero_workers_to_one() {
        let pool = GhostCreatorPool::new(0);
        assert_eq!(pool.worker_count(), 1);
    }

    #[test]
    fn default_size_is_at_least_one() {
        // `clamp(cpus / 2, 1, 4)` → 1 on single-core hosts, up to 4.
        let n = GhostCreatorPool::default_size();
        assert!((1..=4).contains(&n));
    }

    #[test]
    fn drop_joins_workers_cleanly() {
        // Workers should observe sender disconnect and exit; the
        // implicit join in `Drop` ensures we don't leak threads.
        let pool = GhostCreatorPool::new(3);
        // No jobs submitted; just drop and ensure no hang.
        drop(pool);
    }

    // submit() round-trip + execute_ghost_job integration tests need a
    // real `Arc<Engine>` and live in `engine::tests` (PASO 6.2 wiring).

    #[test]
    fn singleflight_guard_drop_removes_hash() {
        let inflight = Arc::new(DashSet::new());
        let hash = 0xdead_beef_dead_beef_u64;
        inflight.insert(hash);
        assert!(inflight.contains(&hash));

        let guard = SingleflightGuard::new(inflight.clone(), hash);
        drop(guard);
        assert!(!inflight.contains(&hash));
    }

    #[test]
    fn singleflight_guard_drop_runs_on_panic() {
        // RAII contract: even if the worker panics mid-execution, the
        // guard's Drop must run during stack unwind so the hash is
        // freed and a future candidate with the same filter_desc can
        // re-enter.
        let inflight = Arc::new(DashSet::new());
        let hash = 0x1234_5678_9abc_def0_u64;
        inflight.insert(hash);

        // `Arc<DashSet>` is not auto-`UnwindSafe` due to interior
        // mutability (sharded RwLocks); `AssertUnwindSafe` is correct
        // here because the guard's Drop is the only state mutation
        // and that mutation is exactly what we're verifying.
        let inflight_for_closure = inflight.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = SingleflightGuard::new(inflight_for_closure, hash);
            panic!("simulated worker panic mid-execute_ghost_job");
        }));
        assert!(result.is_err(), "panic should propagate to catch_unwind");
        assert!(
            !inflight.contains(&hash),
            "guard's Drop must run during panic unwind"
        );
    }

    #[test]
    fn singleflight_guard_distinct_hashes_independent() {
        // Two distinct hashes are tracked independently; dropping one
        // guard does not affect the other.
        let inflight = Arc::new(DashSet::new());
        let h1 = 1u64;
        let h2 = 2u64;
        inflight.insert(h1);
        inflight.insert(h2);

        let guard1 = SingleflightGuard::new(inflight.clone(), h1);
        drop(guard1);

        assert!(!inflight.contains(&h1));
        assert!(inflight.contains(&h2), "h2 untouched by h1's guard drop");
    }
}
