// SPDX-License-Identifier: BUSL-1.1
use super::*;

impl Engine {
    /// Enable the RecordCache with the given budget in bytes. Call before serving requests.
    pub fn set_record_cache_size(&mut self, budget_bytes: usize) {
        if budget_bytes > 0 {
            self.record_cache = Some(RecordCache::new(budget_bytes));
        }
    }

    /// Set the per-`NEAREST` time budget in milliseconds (`0` disables the
    /// airbag). Call before serving requests. See [`XyzError::NearestBudgetExceeded`].
    pub fn set_nearest_budget_ms(&mut self, ms: u64) {
        self.nearest_budget_ms = ms;
    }

    /// Override the auto-ghost detection thresholds. Intended for operators
    /// who want to tune sensitivity for their workload — or disable
    /// auto-ghost entirely by passing a prohibitive latency (e.g. `f64::MAX`
    /// or `1e9`), which is the baseline used by the Zipf benchmark matrix
    /// to isolate the cost of *having* auto-ghost enabled from the wins it
    /// produces. Each argument is applied independently; pass `None` to
    /// keep the current value (default or prior override).
    pub fn set_auto_ghost_thresholds(&self, min_hits: Option<u64>, min_latency_ms: Option<f64>) {
        let mut tel = self.scan_telemetry.write();
        if let Some(h) = min_hits {
            tel.set_min_hits(h);
        }
        if let Some(ms) = min_latency_ms {
            tel.set_min_latency_ms(ms);
        }
    }

    /// Create a hot snapshot of the underlying database. v0.4 cp 3.2.1.
    /// Thin delegate to `turba_engine::engine::TurbaEngine::create_snapshot`;
    /// see that method for the writer-blocking contract and BULKMODE caveat.
    pub fn create_snapshot(
        &self,
        name: &str,
    ) -> std::result::Result<turba_engine::snapshot::SnapshotMeta, turba_engine::error::Error> {
        self.turba.create_snapshot(name)
    }

    /// Test-only: release the underlying data-dir lock without a clean
    /// shutdown (delegates to the turba engine). For crash-simulation tests
    /// that leak this `Engine` via `std::mem::forget`. See
    /// `TurbaEngine::_test_release_dir_lock`.
    pub fn _test_release_dir_lock(&self) {
        self.turba._test_release_dir_lock();
    }

    /// Test-only crash simulation: stop + join every background thread WITHOUT
    /// the graceful flush, then release the dir lock (delegates to the turba
    /// engine). Unlike `_test_release_dir_lock` alone, this leaves NO ghost
    /// thread alive to flush a lagging keyspace after the "crash" — the
    /// faithful SIGKILL semantics a durability test needs (`mem::forget` alone
    /// leaves the bg workers running). Follow with `std::mem::forget(engine)`.
    pub fn _test_crash_stop(&self) {
        self.turba._test_crash_stop();
    }

    /// Flush derived state and record a clean shutdown.
    ///
    /// The ghost index is maintained incrementally in the ghost keyspace
    /// memtable without going through the WAL, so this flushes it (and the ghost
    /// metadata) to durable SSTs and THEN writes the clean-shutdown marker — the
    /// order is load-bearing. On the next open a present marker lets the
    /// persisted index be trusted; its absence triggers a rebuild from the
    /// records (see [`Self::recover_ghosts_after_unclean_shutdown`]).
    ///
    /// Idempotent. [`Drop`] calls this, but any clean-exit path that skips
    /// destructors — `std::process::exit`, a signal handler — MUST call it
    /// explicitly, otherwise every restart needlessly rebuilds the ghosts.
    pub fn shutdown(&self) {
        self.persist_total_writes();

        self.turba.ghosts.seal_active();
        let _ = self.turba.ghosts.flush_sealed();
        self.turba.dictionary.seal_active();
        let _ = self.turba.dictionary.flush_sealed();

        if let Ok(f) = std::fs::File::create(self.clean_shutdown_marker()) {
            let _ = f.sync_all();
        }
        Self::fsync_dir(&self.meta_path);
    }

    /// Complete graceful shutdown for a signal handler or any non-unwinding exit
    /// (`std::process::exit`): everything [`Self::shutdown`] does (persist writes,
    /// seal+flush ghosts/dictionary, clean-shutdown marker) PLUS turba's flush of
    /// the data trees (spatial + vectors) and WAL-thread stop, which otherwise run
    /// only in `Drop` — and `Drop` never runs when a signal kills the process.
    /// Call this, then `std::process::exit`, so `Drop` does not re-run the work.
    ///
    /// Idempotent. The Engine-level bits (clean marker) run first, while the WAL
    /// threads are still live; turba's shutdown then seals the remaining trees and
    /// stops the WAL. The clean-shutdown marker gates only the ghost rebuild;
    /// spatial/vectors are WAL-durable regardless, so flushing them here shortens
    /// the next restart's replay rather than being a durability requirement.
    pub fn graceful_shutdown(&self) {
        self.shutdown();
        if let Err(e) = self.turba.shutdown() {
            tracing::error!("graceful_shutdown: turba flush failed: {e}");
        }
    }

    /// V3: Get the configured durability mode.
    pub fn durability_mode(&self) -> DurabilityMode {
        self.durability
    }

    /// V3: Manually persist the journal (for batched/async modes).
    /// In durable mode, this is a no-op (auto-persisted per write).
    /// Flush + fsync the WAL for non-Durable modes (the Batched-timer / Async
    /// periodic persist). Returns the fsync result instead of swallowing it:
    /// on EIO, `turba.persist()` poisons the WAL (subsequent commits fail fast,
    /// 3a parity) AND this returns `Err` so the caller (the Batched flush timer)
    /// surfaces the failure rather than silently never persisting (5a/5b S1).
    pub fn persist_journal(&self) -> Result<()> {
        if self.durability != DurabilityMode::Durable {
            self.turba
                .persist()
                .map_err(|e| XyzError::Storage(format!("persist journal fsync failed: {e}")))?;
        }
        Ok(())
    }

    // ── Registry persistence ──────────────────────────────────────────────

    pub(crate) fn persist_lobe_registry(&self, lobes: &LobeRegistry) -> Result<()> {
        let bytes = lobes.to_bytes();
        Self::write_file_durable(&self.meta_path.join("lobes.bin"), &bytes)
            .map_err(|e| XyzError::Storage(format!("failed to persist lobe registry: {e}")))
    }

    pub(crate) fn persist_anchor_registry(&self, anchors: &AnchorRegistry) -> Result<()> {
        let bytes = anchors.to_bytes();
        Self::write_file_durable(&self.meta_path.join("anchors.bin"), &bytes)
            .map_err(|e| XyzError::Storage(format!("failed to persist anchor registry: {e}")))
    }

    /// Persist total_writes counters for all lobes with ghost routers.
    pub(crate) fn persist_total_writes(&self) {
        let routers = self.ghost_routers.read();
        for (lobe_id, router) in routers.iter() {
            let writes = router.total_writes();
            if writes > 0 {
                let _ = GhostLobeManager::persist_total_writes(
                    &self.turba.dictionary,
                    *lobe_id,
                    writes,
                );
            }
        }
    }
}
