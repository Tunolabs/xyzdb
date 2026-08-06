// SPDX-License-Identifier: BUSL-1.1
use super::*;
use crate::keyspaces;
use crate::throttle::ThrottleConfig;

impl Engine {
    /// Open or create a database at the given path with default throttle.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_config(path, ThrottleConfig::default(), None, None)
    }

    /// Open with a specific throttle profile (backwards-compatible).
    pub fn open_with_throttle(path: &Path, throttle_config: ThrottleConfig) -> Result<Self> {
        Self::open_with_config(path, throttle_config, None, None)
    }

    /// Open with throttle profile, optional cache size, storage profile, and durability mode.
    pub fn open_with_config(
        path: &Path,
        throttle_config: ThrottleConfig,
        cache_size_bytes: Option<u64>,
        storage_profile: Option<keyspaces::StorageProfile>,
    ) -> Result<Self> {
        // v0.4 cp 4.2.1: lane-aware admission default-on for back-compat
        // with v0.4+ defaults. Test/legacy callers via this entry point.
        Self::open_full(
            path,
            throttle_config,
            cache_size_bytes,
            storage_profile,
            None,
            None,
            None,
            true,
            None,
            turba_engine::memory_budget::ResolvedBudget::default(),
        )
    }

    /// Full configuration open. `io_scheduler` defaults to
    /// [`keyspaces::IoSchedulerMode::Ssd`] (Passthrough). Cycle doc §6 D6.
    /// `l0_batch_override` (H2.3 §9.3) overrides the storage-profile L0
    /// batch default at advanced-tuning request from `xyzdb-server
    /// --l0-batch <N>`. `None` = use the storage-profile default.
    /// `wal_path` (v0.5.2 B.5) optionally relocates the WAL to a path
    /// outside the data dir; `None` keeps the historical
    /// `<path>/journal.wal` co-location.
    ///
    /// `memory_budget` carries the resolved memory budget (bytes + source)
    /// for the engine config; `cache_size_bytes`, when `Some`, still
    /// overrides the derived cache — the caller derives it from the budget
    /// when unset.
    ///
    /// Data lives under `path` (single-tier / fintech layout).
    #[allow(clippy::too_many_arguments)]
    pub fn open_full(
        path: &Path,
        throttle_config: ThrottleConfig,
        cache_size_bytes: Option<u64>,
        storage_profile: Option<keyspaces::StorageProfile>,
        durability: Option<DurabilityMode>,
        io_scheduler: Option<keyspaces::IoSchedulerMode>,
        l0_batch_override: Option<usize>,
        block_cache_lane_admission: bool,
        wal_path: Option<std::path::PathBuf>,
        memory_budget: turba_engine::memory_budget::ResolvedBudget,
    ) -> Result<Self> {
        let path_str = path
            .to_str()
            .ok_or_else(|| XyzError::Storage("Database path is not valid UTF-8".into()))?;

        let cache = cache_size_bytes.unwrap_or(keyspaces::DEFAULT_CACHE_SIZE);
        let profile = storage_profile.unwrap_or_default();
        let durability_mode = durability.unwrap_or_default();
        let io_scheduler_mode = io_scheduler.unwrap_or_default();

        // For batched/async modes, use manual journal persist (no auto-fsync per write)
        let manual_persist = matches!(
            durability_mode,
            DurabilityMode::Batched | DurabilityMode::Async
        );
        let turba = keyspaces::open_engine(
            path_str,
            cache,
            manual_persist,
            profile,
            io_scheduler_mode,
            l0_batch_override,
            block_cache_lane_admission,
            wal_path,
            memory_budget,
        )?;

        let meta_path = path.join("meta");
        std::fs::create_dir_all(&meta_path)
            .map_err(|e| XyzError::Storage(format!("failed to create meta directory: {e}")))?;

        let lobe_registry = load_or_default::<LobeRegistry>(&meta_path.join("lobes.bin"))?;
        let anchor_registry = load_or_default::<AnchorRegistry>(&meta_path.join("anchors.bin"))?;

        let mut ghost_manager = GhostLobeManager::new();
        ghost_manager.set_keyspace_arc(std::sync::Arc::clone(&turba.ghosts));
        // Lightweight ghosts (0.7.6) store group rollups in the dictionary
        // keyspace; the manager needs the handle for reads + RMW.
        ghost_manager.set_dictionary_arc(std::sync::Arc::clone(&turba.dictionary));

        // Restore persisted ghosts from dictionary
        let restored_ghosts = ghost_manager.load_all(&turba.dictionary)?;

        // Build ghost routers from persisted metadata (no record I/O at boot).
        // Extract each ghost's routing fields under its own shard lock (via
        // `with_ghost`), then register — no single lock held across all ghosts.
        let mut routers: HashMap<u16, GhostRouter> = HashMap::new();
        for (lobe_id, ghost_name, _index_count) in &restored_ghosts {
            // v0.2: every filter op is preserved (Eq, Gt, Contains, …). Flat-AND
            // coverage tuples (empty for a non-flat OR/NOT ghost, which routes
            // only by structural equality — see the router).
            #[allow(clippy::type_complexity)]
            let Some((
                filter_fields,
                filter_expr,
                order_by_field,
                sort_inverted,
                has_aggregates,
                group_fields,
                has_projection,
                aggregate_sig,
            )): Option<(
                Vec<(
                    String,
                    xyzdb_core::record::FilterOp,
                    xyzdb_core::value::Value,
                )>,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
            )> = ghost_manager.with_ghost(ghost_name, |meta| {
                let filter_fields = meta
                    .filter
                    .as_flat_and()
                    .map(|conds| {
                        conds
                            .iter()
                            .map(|f| {
                                (
                                    f.field.clone(),
                                    crate::ops::convert_filter_op(&f.op),
                                    crate::ops::literal_to_value(&f.value),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let aggregate_sig = meta
                    .aggregate
                    .as_ref()
                    .map(|a| crate::aggregate_state::aggregate_signature(&a.aggregate_specs))
                    .unwrap_or_default();
                (
                    filter_fields,
                    meta.filter.clone(),
                    meta.order_by_field.clone(),
                    meta.sort_inverted,
                    meta.is_aggregate(),
                    meta.group_fields().to_vec(),
                    meta.has_projection(),
                    aggregate_sig,
                )
            })
            else {
                continue;
            };

            let router = routers.entry(*lobe_id).or_default();
            router.register_ghost(
                ghost_name,
                filter_fields,
                order_by_field,
                sort_inverted,
                has_aggregates,
                group_fields,
            );
            router.set_filter(ghost_name, filter_expr);
            router.set_has_projection(ghost_name, has_projection);
            router.set_aggregate_sig(ghost_name, aggregate_sig);
            tracing::info!("Router: restored ghost '{}' (lobe={})", ghost_name, lobe_id);
        }

        // Restore persisted total_writes per lobe
        for (_, config) in lobe_registry.all() {
            let writes = GhostLobeManager::load_total_writes(
                &turba.dictionary,
                config.id,
                turba.recovered_from_wal(),
            );
            if writes > 0 {
                let router = routers.entry(config.id).or_default();
                router.set_total_writes(writes);
            }
        }

        // Restore pinned fields and dictionary encodings from dictionary
        let pinned_fields = Self::load_pinned_fields(&turba.dictionary, &lobe_registry);
        let (gravity_fields, gravity_needs_migration) =
            Self::load_gravity_fields(&turba.dictionary, &lobe_registry);
        let vector_fields = Self::load_vector_fields(&turba.dictionary, &lobe_registry);
        let satellite_fields = Self::load_satellite_fields(&turba.dictionary, &lobe_registry);
        if gravity_needs_migration {
            tracing::warn!(
                "gravity: pre-D1 (name+value) spec slot(s) detected — data ops are blocked \
                 until `migrate` rehashes to the value-only convention"
            );
        }
        let dict_store = DictRegistry::load_all(&turba.dictionary, &lobe_registry);

        // V5 Fase 2: Load field registries for V2 on-disk format
        let field_registry = LobeFieldRegistry::load_from_disk(&turba.dictionary);

        // Set zone map builder on spatial tree for compaction output
        turba.spatial.set_zone_map_builder(std::sync::Arc::new(
            crate::zone_map::XyzZoneMapBuilder {
                tracked_fields: vec![],     // empty = track all fields
                source_lobe: String::new(), // spatial has mixed lobes, source detected per-record
            },
        ));

        // Set the rollup merge operator on the dictionary tree (hilo B): ghost
        // rollups under the [ROLLUP] prefix are written as blind delta-appends
        // and folded here at compaction + read; all other dictionary keys
        // (anchors, gravity specs, pins) keep last-writer-wins.
        turba.dictionary.set_merge_operator(std::sync::Arc::new(
            crate::rollup_merge::RollupMergeOperator,
        ));

        // 2a: advance + durably persist the per-open boot epoch, then install
        // it BEFORE any LID is minted (persist-before-use). The commit fsyncs
        // in Durable mode, so a crash cannot reuse an epoch; the epoch lands in
        // each LID's low 16 bits, making LIDs from different opens collision-
        // proof even across a backward clock + reset sequence.
        let boot_epoch = {
            let prev = turba
                .dictionary
                .get(&BOOT_EPOCH_KEY)
                .ok()
                .flatten()
                .filter(|v| v.len() == 2)
                .map(|v| u16::from_be_bytes([v[0], v[1]]))
                .unwrap_or(0);
            let epoch = prev.wrapping_add(1);
            let mut batch = turba.batch();
            batch.put_dictionary(&BOOT_EPOCH_KEY, &epoch.to_be_bytes());
            batch
                .commit()
                .map_err(|e| XyzError::Storage(format!("boot-epoch persist: {e}")))?;
            epoch
        };
        xyzdb_core::lid::LID::set_boot_epoch(boot_epoch);

        let engine = Self {
            turba,
            lobe_registry: RwLock::new(lobe_registry),
            anchor_registry: RwLock::new(anchor_registry),
            ghost_manager,
            throttle: WriteThrottle::new(throttle_config),
            ghost_routers: RwLock::new(routers),
            scan_telemetry: RwLock::new(ScanTelemetryRegistry::new()),
            pinned_fields: RwLock::new(pinned_fields),
            gravity_specs: RwLock::new(gravity_fields),
            keel_health: RwLock::new(HashMap::new()),
            keel_omit_warn_ratio: std::env::var("XYZDB_KEEL_OMIT_WARN_RATIO")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|r| (0.0..=1.0).contains(r))
                .unwrap_or(0.01),
            vector_fields: RwLock::new(vector_fields),
            satellite_specs: RwLock::new(satellite_fields),
            gravity_needs_migration: std::sync::atomic::AtomicBool::new(gravity_needs_migration),
            dict_store: RwLock::new(dict_store),
            field_registry: RwLock::new(field_registry),
            record_cache: None,      // Enabled via set_record_cache_size()
            nearest_budget_ms: 3000, // M2.2 airbag, calibrated (see field doc); --nearest-budget-ms overrides
            durability: durability_mode,
            meta_path,
            weak_self: std::sync::OnceLock::new(),
            ghost_candidate_total_count: std::sync::atomic::AtomicU64::new(0),
            ghost_candidate_spawn_count: std::sync::atomic::AtomicU64::new(0),
            ghost_dedup_lost_count: std::sync::atomic::AtomicU64::new(0),
            ghost_singleflight_skipped_count: std::sync::atomic::AtomicU64::new(0),
            ghost_create_failed_other_count: std::sync::atomic::AtomicU64::new(0),
            ghost_pool_submit_failed_count: std::sync::atomic::AtomicU64::new(0),
            ghost_pool: crate::ghost_pool::GhostCreatorPool::new(
                crate::ghost_pool::GhostCreatorPool::default_size(),
            ),
            ghost_inflight: std::sync::Arc::new(dashmap::DashSet::new()),
            bulk_loading: std::sync::atomic::AtomicBool::new(false),
            anchor_shard_locks: (0..ANCHOR_SHARDS)
                .map(|_| parking_lot::Mutex::new(()))
                .collect(),
        };

        // Ghost indexes are updated incrementally into the ghost memtable
        // without going through the WAL; a crash can leave them stale vs the
        // durable records. Rebuild them when the previous shutdown was not
        // clean. See `recover_ghosts_after_unclean_shutdown`.
        engine.recover_ghosts_after_unclean_shutdown()?;

        Ok(engine)
    }

    /// Consume the engine and return it wrapped in `Arc`, with `weak_self`
    /// initialized so lifecycle methods can spawn background threads that
    /// hold their own `Arc<Engine>`. Also starts the TTL reaper thread.
    ///
    /// This is what server/main and integration tests should call once
    /// they're done mutating the engine (e.g. after `set_record_cache_size`).
    /// Integration tests that never spawn lifecycle threads can keep using
    /// `Engine::open(..)` directly and skip this.
    pub fn into_arc(self) -> std::sync::Arc<Self> {
        let arc = std::sync::Arc::new(self);
        let weak = std::sync::Arc::downgrade(&arc);
        let _ = arc.weak_self.set(weak.clone());
        Self::spawn_ttl_reaper(weak);
        arc
    }

    /// Spawn the TTL reaper. Exits automatically when the last `Arc<Engine>`
    /// is dropped (the held `Weak<Engine>` stops upgrading) — no explicit
    /// shutdown channel, no `AtomicBool`, no need for `Engine::shutdown`.
    ///
    /// The tick loop uses 1s sleeps with an upgrade check between each
    /// (shutdown latency ≤ 1s, acceptable for tests) and runs the actual
    /// `reap_cycle` every 60 ticks (~once per minute). The full cycle
    /// includes both TTL drops and — if the UTC day bucket advanced — a
    /// bitmap rotation across all ghosts.
    fn spawn_ttl_reaper(weak: std::sync::Weak<Engine>) {
        let _ = std::thread::Builder::new()
            .name("ghost-ttl-reaper".into())
            .spawn(move || {
                // Bootstrap: assume "today" at thread start, so the first
                // rotation fires at the next UTC midnight rather than at
                // the first tick. `load_all` already reset bitmaps at boot,
                // so initializing here keeps the semantics aligned.
                let mut last_rotation = crate::ghost::now_micros() / crate::ghost::MICROS_PER_DAY;

                loop {
                    // 60 × 1s sleeps with an upgrade check each time —
                    // drop-driven shutdown returns within ~1s of the last
                    // `Arc<Engine>` being released.
                    for _ in 0..60 {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        if weak.strong_count() == 0 {
                            tracing::debug!("ghost-ttl-reaper: engine dropped, exiting");
                            return;
                        }
                    }

                    let Some(engine) = weak.upgrade() else {
                        tracing::debug!("ghost-ttl-reaper: engine dropped between ticks");
                        return;
                    };

                    let current_day_bucket =
                        crate::ghost::now_micros() / crate::ghost::MICROS_PER_DAY;
                    engine.reap_cycle(current_day_bucket, &mut last_rotation);
                    drop(engine); // release the Arc before the next sleep
                }
            });
    }

    /// Rebuild ghosts from the durable records when the previous shutdown was
    /// not clean.
    ///
    /// Ghost indexes are maintained incrementally by `GhostLobeManager::
    /// notify_write`, which writes straight into the ghost keyspace memtable and
    /// bypasses the WAL (ghost.rs). A clean shutdown flushes that memtable to an
    /// SST (see `Drop`), so the persisted index is consistent and `load_all` can
    /// trust it. A crash can lose the unflushed tail, leaving the index stale
    /// relative to the WAL-durable records — and `load_all` does not rebuild. So
    /// when the clean-shutdown marker is absent (a crash, or the very first
    /// boot) every ghost is rebuilt from the records; one that cannot be rebuilt
    /// is dropped, so queries fall back to the primary keyspace (correct, just
    /// slower) instead of serving a stale view.
    ///
    /// A missing marker only ever costs a spurious rebuild (safe); it never
    /// skips a needed one.
    ///
    /// # Errors
    ///
    /// Currently never returns an error: a ghost that fails to rebuild is
    /// downgraded to a drop, so `open` always succeeds. The `Result` is kept so
    /// a future policy could choose to make an unrecoverable ghost fatal.
    fn recover_ghosts_after_unclean_shutdown(&self) -> Result<()> {
        let marker = self.clean_shutdown_marker();
        if marker.exists() {
            // Previous shutdown was clean -> trust the persisted ghost index.
            // Clear the marker durably so a crash from here on is detected.
            let _ = std::fs::remove_file(&marker);
            Self::fsync_dir(&self.meta_path);
            return Ok(());
        }

        let names: Vec<String> = self
            .ghost_manager
            .list()
            .iter()
            .map(|g| g.name.clone())
            .collect();
        for name in names {
            if let Err(e) = self.execute_refresh_ghost(&name) {
                tracing::warn!(
                    "ghost '{name}' rebuild after unclean shutdown failed: {e}; dropping it"
                );
                let _ = self.execute_drop_ghost(&name);
            }
        }
        Ok(())
    }
}
