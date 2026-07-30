use super::*;

impl Engine {
    /// Enforce a per-(lobe, type) ghost limit by evicting the LRU ghost
    /// when the count would exceed `max`. Handles both the ghost-manager
    /// side (drop + persistence cleanup) and the router side
    /// (unregistration) as one atomic-at-the-call-site operation.
    ///
    /// Also clears the evicted ghost's `filter_desc` from the scan
    /// telemetry store, so the pattern can re-trigger a fresh auto-ghost
    /// if it stays hot after a restart-less eviction (the "weekly report
    /// ghost got LRU-evicted; same query next week should get a new
    /// ghost" scenario).
    ///
    /// Called by the auto-ghost creation path (20-Ephemeral cap;
    /// bumped 10 → 20 in v0.2.1 Finding 5) and by the promotion path
    /// (5-Promoted cap). `None` return means either no eviction
    /// was needed or the victim failed to drop cleanly — either way, the
    /// caller just proceeds.
    pub(crate) fn enforce_ghost_type_limit(
        &self,
        lobe_id: u16,
        source_lobe: &str,
        ghost_type: crate::ghost::GhostType,
        max: usize,
    ) -> Option<crate::ghost::ExpiredGhost> {
        let evicted = self.ghost_manager.evict_lru_at_limit(
            source_lobe,
            ghost_type,
            max,
            &self.turba.dictionary,
        )?;

        // Recover filter_desc from the router BEFORE unregistering, so we
        // can clear the telemetry pattern flag afterwards.
        let filter_desc = {
            let routers = self.ghost_routers.read();
            routers
                .get(&lobe_id)
                .and_then(|r| r.get_filter_desc(&evicted.name))
                .map(str::to_owned)
        };

        {
            let mut routers = self.ghost_routers.write();
            if let Some(router) = routers.get_mut(&lobe_id) {
                router.unregister_ghost(&evicted.name);
            }
        }

        if let Some(desc) = filter_desc {
            self.scan_telemetry.write().set_ghost_flag(&desc, false);
        }

        tracing::info!(
            "LRU eviction: dropped {:?} '{}' from lobe '{}' (limit {})",
            ghost_type,
            evicted.name,
            source_lobe,
            max
        );
        Some(evicted)
    }

    /// Run one reaper pass. Three phases, in order:
    ///
    ///   1. Drop every expired ghost (TTL reached). For each, unregister
    ///      from the per-lobe router AND clear the telemetry pattern's
    ///      `ghost_created` flag so the filter can re-trigger if it
    ///      stays hot — covers the "weekly/monthly report" scenario
    ///      where the pattern re-heats after the Ephemeral's 24h TTL
    ///      has long elapsed.
    ///   2. Rotate the daily access bitmap if the UTC day bucket has
    ///      advanced since `last_rotation`. Bit 0 (today) slides to
    ///      bit 1; bit 6 slides to bit 7 (ignored by promotion).
    ///   3. Promote any Ephemeral whose 7-day access bitmap is fully
    ///      set (bits 0-6 all 1, i.e. seven consecutive days of
    ///      access). Promotion is in-place (rename + reclassify + re-
    ///      persist) so the covering index survives without a spatial
    ///      re-scan. The promoted ghost keeps the same filter_desc in
    ///      the router, so the pattern's flag stays set — the filter
    ///      is still covered.
    ///
    /// Called by both the background thread and the `#[cfg(test)]`
    /// suite — the latter passes synthetic `current_day_bucket` values
    /// so rotation is deterministic without waiting on wall clock time.
    pub(crate) fn reap_cycle(&self, current_day_bucket: i64, last_rotation: &mut i64) {
        // Phase 1: drop expired, cascade unregister + clear telemetry flag.
        let dropped = self
            .ghost_manager
            .drop_expired_ghosts(&self.turba.dictionary);
        if !dropped.is_empty() {
            // Recover filter_desc for each dropped ghost BEFORE unregistering.
            let descs: Vec<(u16, String, Option<String>)> = {
                let routers = self.ghost_routers.read();
                dropped
                    .iter()
                    .map(|eg| {
                        let desc = routers
                            .get(&eg.lobe_id)
                            .and_then(|r| r.get_filter_desc(&eg.name))
                            .map(str::to_owned);
                        (eg.lobe_id, eg.name.clone(), desc)
                    })
                    .collect()
            };

            {
                let mut routers = self.ghost_routers.write();
                for (lobe_id, name, _) in &descs {
                    if let Some(router) = routers.get_mut(lobe_id) {
                        router.unregister_ghost(name);
                    }
                }
            }

            let mut telemetry = self.scan_telemetry.write();
            for (_, _, desc) in &descs {
                if let Some(d) = desc {
                    telemetry.set_ghost_flag(d, false);
                }
            }
        }

        // Phase 2: rotate the daily access bitmap if the day advanced.
        self.ghost_manager
            .rotate_bitmaps_if_needed(current_day_bucket, last_rotation);

        // Phase 3: promote Ephemerals with 7 consecutive access days.
        for old_name in self.ghost_manager.identify_promotable() {
            let Some((source_lobe, lobe_id)) = self
                .ghost_manager
                .with_ghost(&old_name, |m| (m.source_lobe.clone(), m.lobe_id))
            else {
                continue; // evicted between identify and here
            };

            // Cap Promoted at 5 per lobe — LRU-evict the oldest if at limit.
            self.enforce_ghost_type_limit(
                lobe_id,
                &source_lobe,
                crate::ghost::GhostType::Promoted,
                5,
            );

            let new_name = match self
                .ghost_manager
                .promote_ghost(&old_name, &self.turba.dictionary)
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("promotion of '{old_name}' failed: {e}");
                    continue;
                }
            };

            // Router: rename the registration in place — filter_fields,
            // filter_desc, state_ready all travel with it. Pattern's
            // `ghost_created` stays true (the new Promoted still covers
            // the same filter).
            let mut routers = self.ghost_routers.write();
            if let Some(router) = routers.get_mut(&lobe_id) {
                router.rename_ghost(&old_name, &new_name);
            }
        }

        // v0.2.2 diagnostic: emit a per-minute tick of engine-wide
        // counters so long-running benchmarks can show whether any
        // in-memory structure grows monotonically. Cheap — read locks,
        // one directory listing per tree, no allocations beyond the
        // log formatter.
        //
        // Intended as the first draft of what will become the `/stats`
        // endpoint. Stays in production after any specific investigation
        // wraps: the counters are useful for any future scenario and
        // cost effectively nothing.
        //
        // Finding 6 signatures this log was built to distinguish:
        // * L0 grows but compact_ok also climbs ⇒ compact slow vs writes
        // * L0 grows and compact_ok stays flat ⇒ compact stuck
        // * level counts stable but disk_sst grows ⇒ stale SSTables
        //   retained (old SuperVersion references or broken cleanup)
        // * counts stable but VmRSS grows ⇒ BlockCache cap not enforced
        //   or some other structure
        let pattern_count = self.scan_telemetry.read().pattern_count();
        let ghost_count = self.ghost_manager.ghost_count();
        let compact_errors = self.turba.total_compact_errors();
        tracing::info!(
            "reap-cycle: patterns={} ghosts={} compact_errors={}",
            pattern_count,
            ghost_count,
            compact_errors,
        );

        // Separate process heap (VmRSS) from cgroup accounting (anon+file).
        // docker stats reports (anon + active_file − inactive_file) on cgroup
        // v2 — so a VmRSS ≪ cgroup total means the peak is page cache, not heap.
        let vm_rss = read_proc_status_mb("VmRSS").unwrap_or(0);
        let vm_data = read_proc_status_mb("VmData").unwrap_or(0);
        let cg_anon = read_cgroup_stat_mb("anon").unwrap_or(0);
        let cg_file = read_cgroup_stat_mb("file").unwrap_or(0);
        let cg_active_file = read_cgroup_stat_mb("active_file").unwrap_or(0);
        let cg_inactive_file = read_cgroup_stat_mb("inactive_file").unwrap_or(0);
        tracing::info!(
            "reap-cycle:   proc VmRSS={}MB VmData={}MB  cgroup anon={}MB file={}MB active_file={}MB inactive_file={}MB",
            vm_rss,
            vm_data,
            cg_anon,
            cg_file,
            cg_active_file,
            cg_inactive_file,
        );

        // Early-warning signal: operator-visible when heap trends toward the
        // cgroup ceiling. 85% leaves ~1.2 GB headroom on an 8 GB T6, which is
        // enough to abort the run and inspect the breakdown before OOM kill.
        if let Some(limit_mb) = read_cgroup_limit_mb()
            && vm_rss * 100 >= limit_mb * 85
        {
            tracing::warn!(
                "reap-cycle: VmRSS approaching cgroup limit: {}MB / {}MB ({}%)",
                vm_rss,
                limit_mb,
                (vm_rss * 100) / limit_mb.max(1),
            );
        }

        let cache = &self.turba.cache_ref();
        tracing::info!(
            "reap-cycle:   block_cache weight={}MB/{}MB len={} hits={} misses={} meta_cache={}MB",
            cache.current_weight() / (1024 * 1024),
            cache.capacity() / (1024 * 1024),
            cache.len(),
            cache.hits(),
            cache.misses(),
            // Evictable per-SST metadata (zone maps + bloom) resident in the
            // metadata cache. This is the resident-metadata RAM that used to be
            // O(dataset) in the per-tree breakdown below — now bounded by the
            // cache budget. If it sits at the budget with rising block misses,
            // the working set exceeds the budget (raise --metadata-cache-size).
            cache.meta_current_weight() / (1024 * 1024),
        );

        for (name, tree) in [
            ("spatial   ", &self.turba.spatial),
            ("identity  ", &self.turba.identity),
            ("dictionary", &self.turba.dictionary),
            ("ghosts    ", &self.turba.ghosts),
            ("vectors   ", &self.turba.vectors),
        ] {
            let levels = tree.level_table_counts();
            let disk = tree.disk_sst_count();
            let mem_active_mb = tree.active_memtable_size() / (1024 * 1024);
            let sealed = tree.sealed_memtable_count();
            let sealed_mb = tree.sealed_memtable_bytes() / (1024 * 1024);
            let cok = tree.compact_success_count();
            let mok = tree.major_compact_success_count();
            let cerr = tree.compact_error_count();
            let level_summary = levels
                .iter()
                .enumerate()
                .map(|(i, c)| format!("l{i}={c}"))
                .collect::<Vec<_>>()
                .join(" ");
            let version_sum: usize = levels.iter().sum();
            tracing::info!(
                "reap-cycle:   {} {} version_sum={} disk_sst={} mem_active={}MB sealed={}({}MB) compact_ok={} major_ok={} compact_err={}",
                name,
                level_summary,
                version_sum,
                disk,
                mem_active_mb,
                sealed,
                sealed_mb,
                cok,
                mok,
                cerr,
            );

            // Per-keyspace SSTable resident-metadata breakdown. Zone maps and
            // bloom are now evictable (held in the metadata cache — see the
            // `meta_cache` term on the block_cache line above), so they read ~0
            // resident here; only the block `index` is still resident per tree
            // (a later increment makes it cacheable too). Per-level vectors
            // surface skew across levels.
            let mb = tree.memory_breakdown();
            let zm_sum_mb = mb.zone_maps_total() / (1024 * 1024);
            let ix_sum_mb = mb.index_total() / (1024 * 1024);
            let bl_sum_mb = mb.bloom_total() / (1024 * 1024);
            let zm_per_level = mb
                .zone_maps_per_level
                .iter()
                .enumerate()
                .map(|(i, b)| format!("l{i}={}MB", b / (1024 * 1024)))
                .collect::<Vec<_>>()
                .join(" ");
            tracing::info!(
                "reap-cycle:   {}   zone_maps={}MB index={}MB bloom={}MB  zone_maps[{}]",
                name,
                zm_sum_mb,
                ix_sum_mb,
                bl_sum_mb,
                zm_per_level,
            );
        }
    }

    /// Upgrade `weak_self` into an owned `Arc<Engine>`. Callers are methods
    /// on `&self` that need to spawn a background thread. Returns `None`
    /// if the engine was never wrapped via `into_arc` (single-threaded
    /// test path) — the caller is expected to treat that as "skip the
    /// background work" rather than panicking.
    pub(crate) fn self_arc(&self) -> Option<std::sync::Arc<Self>> {
        self.weak_self.get().and_then(|w| w.upgrade())
    }

    /// Spawn a background thread to create an auto-ghost for a hot scan
    /// pattern reported by the telemetry store.
    ///
    /// The ghost is built with the candidate's filters plus (optionally)
    /// Count/Sum specs for every field that telemetry observed in AGGREGATE
    /// pipelines. `create()` scans the spatial keyspace — cheap at small
    /// scale, seconds at scale 1.0 — so this MUST NOT run on the caller's
    /// thread. The source scan returns normally; subsequent matching scans
    /// pick up the ghost once the worker finishes.
    ///
    /// Ghost name is `auto_{lobe}_{xxh3_64(filter_desc):016x}`. Using xxh3
    /// (not std's DefaultHasher) makes the name deterministic across
    /// restarts and across scan threads, so two scans racing on the same
    /// pattern converge on the same name; `GhostLobeManager::create` rejects
    /// duplicates so the loser fails cleanly and the winner's ghost stands.
    ///
    /// Silently skips when `weak_self` isn't set (`into_arc` was never
    /// called — always the case for integration tests that don't need
    /// lifecycle behavior).
    pub(crate) fn maybe_create_ephemeral_ghost(
        &self,
        candidate: crate::scan_telemetry::AutoGhostCandidate,
    ) {
        // Pre-design instrumentation for v0.3.2-ghost-singleflight: every
        // candidate the path sees is counted, regardless of subsequent
        // early-return guards. Pre-fix invariant: candidate_total ≈
        // candidate_spawn (the early-returns below are rare; main loss is
        // the dedup race after spawn).
        self.ghost_candidate_total_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let Some(engine_arc) = self.self_arc() else {
            tracing::debug!(
                "auto-ghost: skipping (engine not wrapped in Arc; lifecycle paths inert)"
            );
            return;
        };

        let lobe_id = {
            let lobes = self.lobe_registry.read();
            match lobes.get(&candidate.lobe) {
                Some(c) => c.id,
                None => {
                    tracing::warn!(
                        "auto-ghost: telemetry reported unknown lobe '{}'",
                        candidate.lobe
                    );
                    return;
                }
            }
        };

        // xxh3_64 of `filter_desc`. Deterministic seed=0,
        // collision-resistant non-cryptographic hash. Already used
        // since v0.2.0-alpha for the auto-ghost name (so two scans
        // racing on the same filter_desc converge on the same name).
        // Future: bump to xxh3_128 if a collision pattern emerges
        // empirically (none observed across v0.2.x cycle benches).
        let hash = xxhash_rust::xxh3::xxh3_64(candidate.filter_desc.as_bytes());
        let name = format!("auto_{}_{:016x}", candidate.lobe, hash);

        // PASO 6.3 single-flight: if another candidate with the same
        // `filter_desc` is already in flight (in the channel or being
        // processed by a worker), bail out silently and increment the
        // skipped counter. The opportunistic semantics — see design
        // doc §5 — are appropriate because the next read on the same
        // pattern will pick up the in-flight winner's ghost via the
        // router; no callers wait.
        if !engine_arc.ghost_inflight.insert(hash) {
            engine_arc
                .ghost_singleflight_skipped_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(
                "auto-ghost: single-flight skip for filter_desc={:?}",
                candidate.filter_desc
            );
            return;
        }

        // We won the single-flight race; construct the guard so the
        // hash is removed from `ghost_inflight` exactly once
        // (worker completion, submit failure, or panic — see
        // `SingleflightGuard` docstring).
        let singleflight_guard =
            crate::ghost_pool::SingleflightGuard::new(engine_arc.ghost_inflight.clone(), hash);

        // Submit job to the bounded ghost-creator pool. The pool's
        // N=clamp(cpus/2,1,4) workers serialise the per-spawn scan +
        // deserialize cost so it can no longer saturate both CPUs at
        // once and starve the read path.
        let job = crate::ghost_pool::GhostCreateJob {
            candidate,
            engine_arc: engine_arc.clone(),
            lobe_id,
            name,
            _singleflight_guard: singleflight_guard,
        };
        if engine_arc.ghost_pool.submit(job) {
            // Submit accepted: a worker will pick this up and run
            // `execute_ghost_job`. Counter increments only on
            // acceptance to preserve the pre-PASO-6.2 semantics where
            // `candidate_spawn` = "subset that reached the spawn
            // site" — now "subset accepted by the pool".
            engine_arc
                .ghost_candidate_spawn_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            // Pool backlog full or sender disconnected (engine drop in
            // flight). The submit() returned the rejected job and
            // dropped it; the SingleflightGuard inside dropped with
            // it, so ghost_inflight is consistent. Telemetry will
            // refire on sustained traffic.
            engine_arc
                .ghost_pool_submit_failed_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!("auto-ghost: pool submit failed (backlog full or shutting down)");
        }
    }

    // ── GHOST LOBES ───────────────────────────────────────────────────────

    pub(super) fn execute_create_ghost(
        &self,
        stmt: xytalk_parser::ast::CreateGhostStmt,
    ) -> Result<QueryResult> {
        let lobes = self.lobe_registry.read();
        let lobe_config = lobes
            .get(&stmt.source_lobe)
            .ok_or_else(|| XyzError::LobeNotFound(stmt.source_lobe.clone()))?;
        let lobe_id = lobe_config.id;
        drop(lobes);

        let fr_guard = self.field_registry.read();
        let fd = fr_guard.get_dict(lobe_id);

        // Convert the parsed AGGREGATE clause to the engine's per-metric list.
        let aggregate_specs = Self::funcs_to_metrics(&stmt.aggregates)?;
        tracing::info!(
            "TRACE[1] CREATE GHOST parsed: name={}, aggregates={:?}, group_by={:?}, specs_count={}",
            stmt.name,
            stmt.aggregates,
            stmt.group_by,
            aggregate_specs.len()
        );

        // Resolve + validate an `ORDER BY <metric>` declaration against the
        // AGGREGATE clause (metric-order only applies to grouped aggregates).
        let metric_order = Self::resolve_metric_order(
            &stmt.order_metric,
            stmt.sort_descending,
            &aggregate_specs,
            &stmt.group_by,
        )?;

        let msg = self.ghost_manager.create(
            &self.ghost_spatial_tree(),
            &self.turba.dictionary,
            lobe_id,
            &stmt.name,
            &stmt.source_lobe,
            stmt.filter,
            &stmt.order_by,
            stmt.sort_descending,
            false,
            aggregate_specs,
            stmt.group_by,
            stmt.embed,
            fd,
            metric_order,
        )?;

        self.ghost_manager.flush()?;
        self.wait_compaction_settle();
        self.register_ghost_in_router(&stmt.name, lobe_id)?;

        Ok(QueryResult::Ok {
            lid: None,
            message: msg,
        })
    }

    /// Batch-create multiple ghosts from the same lobe in a single scan.
    pub(super) fn execute_create_ghost_batch(
        &self,
        source_lobe: &str,
        ghost_list: Vec<xytalk_parser::ast::CreateGhostStmt>,
    ) -> Result<QueryResult> {
        let lobes = self.lobe_registry.read();
        let lobe_config = lobes
            .get(source_lobe)
            .ok_or_else(|| XyzError::LobeNotFound(source_lobe.to_string()))?;
        let lobe_id = lobe_config.id;
        drop(lobes);

        let fr_guard = self.field_registry.read();
        let fd = fr_guard.get_dict(lobe_id);

        let specs: Vec<crate::ghost::GhostSpec> = ghost_list
            .into_iter()
            .map(|gs| {
                let aggregate_specs = Self::funcs_to_metrics(&gs.aggregates)?;
                let metric_order = Self::resolve_metric_order(
                    &gs.order_metric,
                    gs.sort_descending,
                    &aggregate_specs,
                    &gs.group_by,
                )?;
                Ok(crate::ghost::GhostSpec {
                    aggregate_specs,
                    name: gs.name,
                    filter: gs.filter,
                    order_by_field: gs.order_by,
                    sort_inverted: gs.sort_descending,
                    is_auto: false,
                    group_fields: gs.group_by,
                    projection: gs.embed,
                    metric_order,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        tracing::info!(
            "Batch creating {} ghosts from '{}': {:?}",
            specs.len(),
            source_lobe,
            names
        );

        let messages = self.ghost_manager.create_batch(
            &self.ghost_spatial_tree(),
            &self.turba.dictionary,
            lobe_id,
            source_lobe,
            specs,
            fd,
        )?;

        self.ghost_manager.flush()?;

        // Register all created ghosts in router
        for name in &names {
            if let Err(e) = self.register_ghost_in_router(name, lobe_id) {
                tracing::warn!(
                    "Ghost '{}' batch-created but router registration failed: {e}",
                    name
                );
            }
        }

        let msg = messages.join("\n");
        Ok(QueryResult::Ok {
            lid: None,
            message: msg,
        })
    }

    /// Convert a parsed `AGGREGATE` clause to the engine's per-metric list,
    /// validating labels (rejects duplicate labels / a filtered `count()` with
    /// no alias). Uses the SINGLE canonical scheme (`canonical_label`) so a ghost
    /// stores exactly the label a client sees from the runtime path.
    pub(crate) fn funcs_to_metrics(
        aggs: &[xytalk_parser::ast::Aggregate],
    ) -> Result<Vec<crate::aggregate_state::Metric>> {
        use crate::aggregate_state::{AggOp, Metric};
        use xytalk_parser::ast::AggregateFunc as F;

        let labels =
            crate::ops::aggregate::resolve_labels(aggs, crate::ops::aggregate::canonical_label)
                .map_err(XyzError::Parse)?;

        Ok(aggs
            .iter()
            .zip(labels)
            .map(|(a, label)| {
                let (field, op) = match &a.func {
                    F::Count => (String::new(), AggOp::Count),
                    F::Sum(f) => (f.clone(), AggOp::Sum),
                    F::Avg(f) => (f.clone(), AggOp::Avg),
                    F::Min(f) => (f.clone(), AggOp::Min),
                    F::Max(f) => (f.clone(), AggOp::Max),
                };
                Metric::new(field, op, label, a.filter.clone())
            })
            .collect())
    }

    /// Resolve a `CREATE GHOST … ORDER BY <metric>` declaration into a
    /// [`crate::ghost::MetricOrder`], validating it names a metric the AGGREGATE
    /// clause produces. Metric-order applies only to grouped aggregates, so it
    /// requires both GROUP BY and AGGREGATE. `None` when no metric order was
    /// declared (the classic field order path).
    fn resolve_metric_order(
        order_metric: &Option<xytalk_parser::ast::TopBy>,
        descending: bool,
        aggregate_specs: &[crate::aggregate_state::Metric],
        group_by: &[String],
    ) -> Result<Option<crate::ghost::MetricOrder>> {
        use xytalk_parser::ast::TopBy;
        let Some(by) = order_metric else {
            return Ok(None);
        };
        if group_by.is_empty() || aggregate_specs.is_empty() {
            return Err(XyzError::InvalidQuery(
                "ORDER BY <metric> requires GROUP BY and AGGREGATE (metric-order is for grouped \
                 aggregates)"
                    .into(),
            ));
        }
        let label = match by {
            TopBy::Metric(f) => crate::ops::aggregate::canonical_label(f),
            TopBy::Alias(a) => a.clone(),
        };
        if !aggregate_specs.iter().any(|m| m.label == label) {
            return Err(XyzError::InvalidQuery(format!(
                "ORDER BY '{label}': not a metric in the AGGREGATE clause"
            )));
        }
        Ok(Some(crate::ghost::MetricOrder { label, descending }))
    }

    pub(super) fn execute_scan_ghost(
        &self,
        stmt: xytalk_parser::ast::ScanGhostStmt,
    ) -> Result<QueryResult> {
        let limit = stmt.limit.unwrap_or(u64::MAX) as usize;
        let lobes = self.lobe_registry.read();
        // Resolve lobe_id from ghost metadata
        let lobe_id = self
            .ghost_manager
            .with_ghost(&stmt.name, |m| m.lobe_id)
            .unwrap_or(0);
        let fr_guard = self.field_registry.read();
        let fd = fr_guard.get_dict(lobe_id);
        drop(lobes);
        // xyTalk v1 P1: an AND-pure (or absent) WHERE pushes into read_topn's
        // ordered scan with early-out at the limit — the unchanged fast path.
        // OR/NOT can't ride the AND-only pushdown, so read the ordered entries
        // unfiltered, then walker-filter and truncate: the "OR => scan" contract,
        // symmetric with Engine::resolve_find_expr.
        let records = match &stmt.filter_expr {
            None => {
                self.ghost_manager
                    .read_topn(&stmt.name, limit, &[], &self.turba.spatial, fd)?
            }
            Some(expr) => match expr.as_flat_and() {
                Some(flat) => {
                    let flat: Vec<xytalk_parser::ast::Filter> = flat.into_iter().cloned().collect();
                    self.ghost_manager.read_topn(
                        &stmt.name,
                        limit,
                        &flat,
                        &self.turba.spatial,
                        fd,
                    )?
                }
                None => {
                    let core = crate::ops::to_core_expr(expr);
                    self.ghost_manager
                        .read_topn(&stmt.name, usize::MAX, &[], &self.turba.spatial, fd)?
                        .into_iter()
                        .filter(|r| crate::ops::matches_core_expr(r, &core))
                        .take(limit)
                        .collect()
                }
            },
        };
        Ok(QueryResult::Records(records))
    }

    pub(super) fn execute_refresh_ghost(&self, name: &str) -> Result<QueryResult> {
        let info = self.ghost_manager.list();
        let ghost_info = info
            .iter()
            .find(|g| g.name == name)
            .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))?;

        let source = ghost_info.source_lobe.clone();
        let order = ghost_info.order_by.clone();
        // Rebuild from the ghost's own membership filter, re-scanning the source
        // lobe's CURRENT records — never the (possibly stale) ghost index.
        let filter = self.ghost_manager.get_filter(name)?;

        // Read aggregate_specs, group_by, projection and the metric-order
        // declaration from current ghost metadata — REFRESH rebuilds from source
        // and must carry the `ORDER BY <metric>` forward (re-emitting the order),
        // not silently drop it.
        let (aggregate_specs, group_by, projection, metric_order) = self
            .ghost_manager
            .with_ghost(name, |m| {
                (
                    m.aggregate
                        .as_ref()
                        .map_or_else(Vec::new, |a| a.aggregate_specs.clone()),
                    m.group_fields().to_vec(),
                    m.projection.clone(),
                    m.metric_order.clone(),
                )
            })
            .unwrap_or((vec![], vec![], vec![], None));

        self.unregister_ghost_from_router(name);
        self.ghost_manager
            .drop_ghost(name, &self.turba.dictionary)?;

        let lobes = self.lobe_registry.read();
        let lobe_id = lobes.get(&source).map(|l| l.id).unwrap_or(0);
        drop(lobes);

        let fr_guard = self.field_registry.read();
        let fd = fr_guard.get_dict(lobe_id);
        let msg = self.ghost_manager.create(
            &self.ghost_spatial_tree(),
            &self.turba.dictionary,
            lobe_id,
            name,
            &source,
            filter,
            &order,
            false,
            false,
            aggregate_specs,
            group_by,
            projection,
            fd,
            metric_order,
        )?;

        self.register_ghost_in_router(name, lobe_id)?;

        Ok(QueryResult::Ok {
            lid: None,
            message: format!("Refreshed: {msg}"),
        })
    }

    pub(super) fn execute_drop_ghost(&self, name: &str) -> Result<QueryResult> {
        self.unregister_ghost_from_router(name);
        let msg = self
            .ghost_manager
            .drop_ghost(name, &self.turba.dictionary)?;
        Ok(QueryResult::Ok {
            lid: None,
            message: msg,
        })
    }

    // ── Ghost Router integration ───────────────────────────────────────

    fn register_ghost_in_router(&self, ghost_name: &str, lobe_id: u16) -> Result<()> {
        // Extract everything the router needs under this ghost's shard read
        // lock, then release it before touching the router (parity with the old
        // `drop(ghosts)` before router access).
        #[allow(clippy::type_complexity)]
        let (
            filter_fields,
            filter_expr,
            order_by_field,
            sort_inverted,
            has_aggregates,
            group_fields,
            has_projection,
            aggregate_sig,
        ): (
            Vec<(
                String,
                xyzdb_core::record::FilterOp,
                xyzdb_core::value::Value,
            )>,
            xytalk_parser::ast::FilterExpr,
            String,
            bool,
            bool,
            Vec<String>,
            bool,
            Vec<String>,
        ) = self
            .ghost_manager
            .with_ghost(ghost_name, |meta| {
                // v0.2: preserve every filter op through to the router. Flat-AND
                // coverage tuples for the router's subset match; empty for a
                // non-flat (OR/NOT) ghost, which routes only by structural
                // equality (the router guards the empty case).
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
                // Metric signature for the router's metric-match guard.
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
            .ok_or_else(|| {
                XyzError::Internal(format!("Ghost '{}' not found for router", ghost_name))
            })?;

        tracing::info!(
            "TRACE[4] ROUTER register: name={}, lobe_id={}, has_aggregates={}, group_fields={:?}, filter_fields_count={}",
            ghost_name,
            lobe_id,
            has_aggregates,
            &group_fields,
            filter_fields.len()
        );

        let mut routers = self.ghost_routers.write();
        let router = routers.entry(lobe_id).or_default();
        router.register_ghost(
            ghost_name,
            filter_fields,
            order_by_field.clone(),
            sort_inverted,
            has_aggregates,
            group_fields,
        );
        router.set_filter(ghost_name, filter_expr);
        router.set_has_projection(ghost_name, has_projection);
        router.set_aggregate_sig(ghost_name, aggregate_sig);
        Ok(())
    }

    fn unregister_ghost_from_router(&self, ghost_name: &str) {
        let mut routers = self.ghost_routers.write();
        for router in routers.values_mut() {
            router.unregister_ghost(ghost_name);
        }
    }

    /// Wait for sealed memtables to flush and L0 to stabilize.
    fn wait_compaction_settle(&self) {
        for _ in 0..100 {
            let sealed = self.turba.spatial.sealed_memtable_count();
            let l0 = self.turba.spatial.l0_table_count();
            if sealed == 0 && l0 <= 8 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
