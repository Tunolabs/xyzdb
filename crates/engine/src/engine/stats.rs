use super::*;
use crate::stats::{
    BlockCacheStats, CgroupStats, CompactStats, GhostAutoStats, GhostLobeEntry, GhostStats,
    KeyspaceStats, LaneStats, MemoryStats, PageCacheStats, PerTreeBlockCacheStats, ProcessStats,
    RamBudgetSnapshot, SchedulerStats, StatsSnapshot, SyncThreadStats, WarmupStats,
};

impl Engine {
    /// Build a snapshot of engine-wide health metrics. Intended as the
    /// payload for the server's `STATS` short-circuit handler. Uses only
    /// lock-free atomic loads and brief reads on `RwLock`s that are also
    /// touched by existing `SHOW *` commands, so calling this concurrently
    /// with production traffic does not introduce new stall risk. Linux
    /// probes (`VmRSS` / cgroup) return 0 on macOS and other non-Linux
    /// targets — expected.
    pub fn stats_snapshot(&self) -> StatsSnapshot {
        const MB_TO_BYTES: u64 = 1024 * 1024;

        let cache_for_per_tree = self.turba.cache_ref();
        let mut keyspaces = std::collections::BTreeMap::new();
        for (name, tree) in [
            ("spatial", &self.turba.spatial),
            ("identity", &self.turba.identity),
            ("dictionary", &self.turba.dictionary),
            ("ghosts", &self.turba.ghosts),
            ("vectors", &self.turba.vectors),
        ] {
            let level_counts = tree.level_table_counts();
            let mut levels = std::collections::BTreeMap::new();
            for (i, c) in level_counts.iter().enumerate() {
                levels.insert(format!("l{i}"), *c);
            }
            let version_sum: usize = level_counts.iter().sum();
            let mb = tree.memory_breakdown();
            let per_tree = cache_for_per_tree.per_tree_snapshot(tree.tree_id());
            let block_cache = PerTreeBlockCacheStats {
                hits: per_tree.hits,
                misses: per_tree.misses,
                disk_read_us_total: per_tree.disk_read_us_total,
                cache_read_us_total: per_tree.cache_read_us_total,
                avg_disk_read_us: if per_tree.misses > 0 {
                    per_tree.disk_read_us_total as f64 / per_tree.misses as f64
                } else {
                    0.0
                },
                avg_cache_read_us: if per_tree.hits > 0 {
                    per_tree.cache_read_us_total as f64 / per_tree.hits as f64
                } else {
                    0.0
                },
                pread_service_time_us_histogram: per_tree.pread_service_time_buckets,
            };
            let page_residency = tree.page_cache_residency();
            let page_cache = PageCacheStats {
                resident_pages: page_residency.resident_pages,
                total_pages: page_residency.total_pages,
                file_size_bytes: page_residency.file_size_bytes,
                residency_ratio: page_residency.ratio(),
            };
            let tree_warmup = tree.warmup_stats();
            let warmup = WarmupStats {
                wall_ms: tree_warmup.wall_ms,
                bytes_loaded: tree_warmup.bytes_loaded,
                sstables_opened: tree_warmup.sstables_opened,
            };
            keyspaces.insert(
                name.to_string(),
                KeyspaceStats {
                    levels,
                    version_sum,
                    disk_sst: tree.disk_sst_count(),
                    flushed_seqno: tree.flushed_seqno(),
                    memory: MemoryStats {
                        mem_active_bytes: tree.active_memtable_size() as u64,
                        sealed_count: tree.sealed_memtable_count() as u64,
                        sealed_bytes: tree.sealed_memtable_bytes() as u64,
                        zone_maps_bytes: mb.zone_maps_total() as u64,
                        index_bytes: mb.index_total() as u64,
                        bloom_bytes: mb.bloom_total() as u64,
                    },
                    compact: CompactStats {
                        compact_ok: tree.compact_success_count(),
                        major_ok: tree.major_compact_success_count(),
                        compact_err: tree.compact_error_count(),
                        trivial_move_count: tree.trivial_move_count(),
                        trivial_move_bytes_saved: tree.trivial_move_bytes_saved(),
                        prewarm_l0_invocations: tree.prewarm_l0_invocations(),
                        prewarm_l0_bytes_read: tree.prewarm_l0_bytes_read(),
                        prewarm_l0_wall_us: tree.prewarm_l0_wall_us(),
                        prewarm_l0_errors: tree.prewarm_l0_errors(),
                    },
                    block_cache,
                    page_cache,
                    warmup,
                },
            );
        }

        let cache = self.turba.cache_ref();
        // v0.4 cp 4.2.1: surface lane-admission counters per lane.
        let admission_raw = cache.admission_snapshot();
        let admission: [crate::stats::LaneAdmissionStats; turba_engine::io::Lane::COUNT] =
            std::array::from_fn(|i| crate::stats::LaneAdmissionStats {
                admitted: admission_raw[i].0,
                skipped: admission_raw[i].1,
            });
        let block_cache = BlockCacheStats {
            weight_bytes: cache.current_weight(),
            capacity_bytes: cache.capacity(),
            len: cache.len() as u64,
            hits: cache.hits(),
            misses: cache.misses(),
            lane_admission_enabled: cache.lane_admission_enabled(),
            admission,
        };

        let ghost_infos = self.ghost_manager.list();
        let ghosts = GhostStats {
            total: ghost_infos.len() as u64,
            per_lobe: ghost_infos
                .into_iter()
                .map(|g| GhostLobeEntry {
                    name: g.name,
                    source_lobe: g.source_lobe,
                    record_count: g.record_count,
                })
                .collect(),
            auto: GhostAutoStats {
                candidate_total: self
                    .ghost_candidate_total_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                candidate_spawn: self
                    .ghost_candidate_spawn_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                dedup_lost: self
                    .ghost_dedup_lost_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                singleflight_skipped: self
                    .ghost_singleflight_skipped_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                create_failed_other: self
                    .ghost_create_failed_other_count
                    .load(std::sync::atomic::Ordering::Relaxed),
                pool_submit_failed: self
                    .ghost_pool_submit_failed_count
                    .load(std::sync::atomic::Ordering::Relaxed),
            },
        };

        let sync_thread = SyncThreadStats {
            last_successful_sync_ts_ms: self.turba.sync_thread_last_successful_sync_ts_ms(),
            heartbeat_count: self.turba.sync_thread_heartbeat_count(),
        };

        let process = ProcessStats {
            vmrss_bytes: read_proc_status_mb("VmRSS").unwrap_or(0) * MB_TO_BYTES,
            vmdata_bytes: read_proc_status_mb("VmData").unwrap_or(0) * MB_TO_BYTES,
        };
        let cgroup = CgroupStats {
            anon_bytes: read_cgroup_stat_mb("anon").unwrap_or(0) * MB_TO_BYTES,
            file_bytes: read_cgroup_stat_mb("file").unwrap_or(0) * MB_TO_BYTES,
            active_file_bytes: read_cgroup_stat_mb("active_file").unwrap_or(0) * MB_TO_BYTES,
            inactive_file_bytes: read_cgroup_stat_mb("inactive_file").unwrap_or(0) * MB_TO_BYTES,
        };

        // Scheduler snapshot. All five trees share one Arc<Scheduler>;
        // any tree's scheduler() returns the same instance.
        let sched = self.turba.spatial.scheduler();
        let sm = sched.metrics();
        let to_lane_stats =
            |lane: turba_engine::io::Lane, lm: turba_engine::io::LaneMetrics| -> LaneStats {
                // Querying p50_us has the side effect of updating the EWMA
                // and SLO breach counter on the Laned arm. STATS reads are
                // the canonical refresh trigger for these in H1.2.
                let p50_us = sched.p50_us(lane).unwrap_or(0);
                LaneStats {
                    ops: lm.ops,
                    elapsed_us_total: lm.elapsed_us_total,
                    avg_elapsed_us: lm.avg_elapsed_us(),
                    outstanding: lm.outstanding_clamped(),
                    outstanding_peak: lm.outstanding_peak,
                    p50_us,
                    ewma_p50_us: sched.ewma_p50_us(lane),
                    slo_breach_count: sched.slo_breach_count(lane),
                }
            };
        let scheduler = SchedulerStats {
            mode: sched.mode_str().to_string(),
            user_io_read: to_lane_stats(
                turba_engine::io::Lane::UserIORead,
                sm.lane(turba_engine::io::Lane::UserIORead),
            ),
            writer_durable: to_lane_stats(
                turba_engine::io::Lane::WriterDurable,
                sm.lane(turba_engine::io::Lane::WriterDurable),
            ),
            flush: to_lane_stats(
                turba_engine::io::Lane::Flush,
                sm.lane(turba_engine::io::Lane::Flush),
            ),
            compaction: to_lane_stats(
                turba_engine::io::Lane::Compaction { target_level: 0 },
                sm.lane(turba_engine::io::Lane::Compaction { target_level: 0 }),
            ),
            scan: to_lane_stats(
                turba_engine::io::Lane::Scan,
                sm.lane(turba_engine::io::Lane::Scan),
            ),
            cross_lane_outstanding_peak: sched.cross_lane_outstanding_peak(),
        };

        // v0.6.0-pre C.2: RAM budget observer. Per-component byte
        // accounting + ratio against the OS-level VmRSS. Pure
        // observability; no enforcement. The trees themselves include
        // the dictionary keyspace, so memtable + SST metadata sums
        // already cover it — no separate dictionary_bytes accounting.
        let trees = [
            &self.turba.spatial,
            &self.turba.identity,
            &self.turba.dictionary,
            &self.turba.ghosts,
            &self.turba.vectors,
        ];
        let memtables_bytes: u64 = trees
            .iter()
            .map(|t| (t.active_memtable_size() + t.sealed_memtable_bytes()) as u64)
            .sum();
        let sst_metadata_bytes: u64 = trees
            .iter()
            .map(|t| {
                let b = t.memory_breakdown();
                (b.zone_maps_per_level.iter().sum::<usize>()
                    + b.index_per_level.iter().sum::<usize>()
                    + b.bloom_per_level.iter().sum::<usize>()) as u64
            })
            .sum();
        let block_cache_bytes = cache.current_weight();
        let record_cache_bytes = self
            .record_cache
            .as_ref()
            .map(|c| c.used_bytes() as u64)
            .unwrap_or(0);
        // Registries: precise accounting not yet implemented.
        let registries_bytes: u64 = 0;
        // 0.7.5 — ghost aggregate state (group_summaries + globals). The
        // dominant un-modelled term at scale; see RamBudgetSnapshot docs.
        let ghost_aggregates_bytes = self.ghost_manager.aggregate_state_bytes();
        let total_estimated_bytes = block_cache_bytes
            + record_cache_bytes
            + memtables_bytes
            + sst_metadata_bytes
            + registries_bytes
            + ghost_aggregates_bytes;
        let vmrss_bytes = process.vmrss_bytes;
        let ratio = if vmrss_bytes > 0 {
            total_estimated_bytes as f64 / vmrss_bytes as f64
        } else {
            0.0
        };
        let ram_budget = RamBudgetSnapshot {
            block_cache_bytes,
            record_cache_bytes,
            memtables_bytes,
            sst_metadata_bytes,
            registries_bytes,
            ghost_aggregates_bytes,
            total_estimated_bytes,
            vmrss_bytes,
            ratio,
        };

        StatsSnapshot {
            keyspaces,
            block_cache,
            ghosts,
            keel_health: self.keel_health_entries(),
            sync_thread,
            process,
            cgroup,
            scheduler,
            ram_budget,
            invariant_guards: crate::stats::InvariantGuards {
                level_overlap: turba_engine::tree::version::level_overlap_violations(),
                level_overlap_by_keyspace: turba_engine::tree::version::level_overlap_by_keyspace(),
                anchor_bloom_false_negative: crate::ops::put::anchor_bloom_false_negatives(),
            },
            recovered_from_wal: self.turba.recovered_from_wal(),
        }
    }
}
