// SPDX-License-Identifier: BUSL-1.1
use super::*;

impl GhostLobeManager {
    // ─── CRUD ───────────────────────────────────────────────────────────

    /// Create a Ghost Lobe by scanning the spatial keyspace and writing
    /// sort-key → spatial-key references (NOT full record copies).
    ///
    /// Synchronous build (background build comes in Phase 5).
    #[allow(clippy::too_many_arguments)]
    /// `spatial` is the engine's single spatial Tree holding the
    /// source-lobe data. The lobe-prefix scan walks it and
    /// aggregates entries into the ghost SST.
    ///
    /// Ghost SSTs stay on the source-lobe's tree (ghost data does
    /// not migrate).
    pub fn create(
        &self,
        spatial: &Tree,
        dictionary: &Tree,
        lobe_id: u16,
        name: &str,
        source_lobe: &str,
        filter: ast::FilterExpr,
        order_by_field: &str,
        sort_inverted: bool,
        is_auto: bool,
        aggregate_specs: Vec<crate::aggregate_state::Metric>,
        group_fields: Vec<String>,
        projection: Vec<String>,
        field_dict: Option<&xyzdb_core::field_dict::FieldDict>,
        metric_order: Option<MetricOrder>,
    ) -> Result<String> {
        if self.contains_ghost(name) {
            return Err(XyzError::GhostExists(name.to_string()));
        }

        tracing::info!(
            "TRACE[2] create: name={}, aggregate_specs={:?}, group_fields={:?}",
            name,
            aggregate_specs,
            group_fields
        );

        let ks = self.ks()?;
        let ghost_compact_was_enabled = ks.compaction_enabled();
        ks.set_compaction_enabled(false);

        let ghost_id = self.alloc_id();

        // Convert the AST filter tree to the core-typed tree once; membership
        // during build evaluates it with the single walker (handles OR/NOT/In).
        let core_filter = crate::ops::to_core_expr(&filter);

        // Scan spatial keyspace with lobe_id prefix.
        // Ghost entries are written directly to SSTables via ingest_sorted —
        // bypasses the memtable entirely. Memory: O(block_size) constant.
        let prefix = lobe_id.to_be_bytes();
        let mut index_count = 0u64;
        let mut global_aggregates = crate::aggregate_state::AggregateState::default();
        let mut group_summaries: std::collections::BTreeMap<
            String,
            crate::aggregate_state::AggregateState,
        > = std::collections::BTreeMap::new();

        // Stream source spatial entries straight into the ghost SSTable buffer:
        // build each entry, fold the aggregate, and flush the buffer to an
        // SSTable when it reaches the memtable size. Memory is O(buf_limit +
        // groups), NOT O(matching rows). An earlier version materialised every
        // matching record into one `Vec` first, which OOMed the 8 GB envelope on
        // high-cardinality ghosts at scale (e.g. overdue installments over a
        // ~75M-row lobe) — the server thrashed/crashed and REFRESH/CREATE hung.
        // Cross-flush ordering need not be global: ingest_sorted persists each
        // sorted chunk as an L0 SSTable and ghost-keyspace compaction merges
        // them. The byte counter is tracked incrementally (the previous code
        // re-summed the whole buffer on every push — O(n^2)).
        use turba_engine::types::{Entry as TurbaEntry, ValueType};
        let seqno = ks.current_seqno();
        let mut entry_buf: Vec<TurbaEntry> = Vec::with_capacity(100_000);
        let buf_limit = ks.max_memtable_size(); // ~8MB worth of entries
        let mut buf_bytes = 0usize;
        let mut spiller = RollupSpiller::new(ghost_id);

        for entry in spatial
            .prefix_iter(&prefix)
            .map_err(|e| XyzError::Storage(e.to_string()))?
        {
            let record =
                match xyzdb_core::record::deserialize_record(&entry.value, source_lobe, field_dict)
                {
                    Ok(r) => r,
                    Err(_) => continue,
                };
            if !crate::ops::matches_core_expr(&record, &core_filter) {
                continue;
            }
            let sort_value = get_sort_value(&record, order_by_field);
            // Uniqueness tiebreak so entries don't collapse: spatial key per
            // record (covering) or group key per group (grouped).
            let tiebreak = ghost_entry_tiebreak(&group_fields, &record.fields, &entry.key);
            let sort_key = crate::sort_encoding::encode_sort_key(
                ghost_id,
                sort_value,
                sort_inverted,
                &tiebreak,
            );
            let entry_value = encode_ghost_value(&entry.key, &record, &projection);
            buf_bytes += sort_key.len() + entry_value.len() + 20;
            entry_buf.push(TurbaEntry {
                key: sort_key,
                value: entry_value,
                seqno: seqno + index_count + 1,
                value_type: ValueType::Value,
            });

            // Accumulate aggregates. The in-RAM map is bounded: past
            // `group_spill_limit()` groups it spills to the rollup
            // namespace and clears (lightweight ghost) — build memory
            // is O(limit), not O(groups).
            if !aggregate_specs.is_empty() {
                global_aggregates.add(&record, &aggregate_specs);
                if !group_fields.is_empty() {
                    let gk =
                        crate::aggregate_state::extract_group_key(&record.fields, &group_fields);
                    group_summaries
                        .entry(gk)
                        .or_default()
                        .add(&record, &aggregate_specs);
                    spiller.maybe_spill(&mut group_summaries, dictionary)?;
                }
            }

            index_count += 1;

            // Flush to an SSTable when the buffer reaches the target size.
            if buf_bytes >= buf_limit {
                entry_buf.sort_by(|a, b| a.key.cmp(&b.key));
                ks.ingest_sorted(entry_buf.drain(..))
                    .map_err(|e| XyzError::Storage(format!("ghost ingest: {e}")))?;
                buf_bytes = 0;
            }
        }

        // Sort entries by key (sort_key) before ingest — spatial scan yields records
        // in spatial key order which differs from the ghost's sort order.
        entry_buf.sort_by(|a, b| a.key.cmp(&b.key));

        // Flush remaining entries
        if !entry_buf.is_empty() {
            ks.ingest_sorted(entry_buf.drain(..))
                .map_err(|e| XyzError::Storage(format!("ghost ingest: {e}")))?;
        }

        // A ghost that spilled flushes its remainder too — uniformly
        // lightweight (empty in-RAM map IS the lightweight discriminator).
        spiller.finalize(&mut group_summaries, dictionary)?;
        if spiller.spilled() {
            tracing::info!(
                "ghost '{}' is lightweight: group rollups on disk (> {} groups)",
                name,
                group_spill_limit()
            );
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        // Metric-ordered rollup (ORDER BY <metric>): emit a full pass now. These
        // are separate blind inserts the dictionary's compaction sorts into
        // metric order — the record write path is never touched. A spilled ghost
        // sources from the finalized rollups on disk; an in-RAM ghost from the
        // summaries map (borrowed before it moves into `meta`). A collision or
        // error marks the order stale (`None`), so TOP falls back to the O(M)
        // quickselect.
        let order_emitted_at = match &metric_order {
            Some(order) => {
                let in_ram = if spiller.spilled() {
                    None
                } else {
                    Some(&group_summaries)
                };
                match crate::ghost::metric_order::emit_metric_order(
                    dictionary,
                    ghost_id,
                    &group_fields,
                    order,
                    in_ram,
                ) {
                    Ok(true) => Some(now),
                    Ok(false) => {
                        tracing::warn!(
                            "ghost '{name}': metric-order emit hit a tiebreak collision; \
                             order marked stale (TOP falls back to O(M))"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            "ghost '{name}': metric-order emit failed: {e}; order marked stale"
                        );
                        None
                    }
                }
            }
            None => None,
        };

        // All ghosts start as Permanent regardless of `is_auto`. The
        // auto-ghost path reclassifies to Ephemeral and sets a TTL after
        // create() returns. Keeping create() logic-free here means the
        // on-disk default stays consistent with older v0.2.0-dev builds.
        let meta = GhostMeta {
            name: name.to_string(),
            ghost_id,
            version: 2,
            lobe_id,
            source_lobe: source_lobe.to_string(),
            filter,
            order_by_field: order_by_field.to_string(),
            sort_inverted,
            metric_order,
            order_emitted_at,
            state: 1, // Ready
            index_count,
            aggregate: if aggregate_specs.is_empty() {
                None
            } else {
                Some(AggregateContent {
                    aggregate_specs,
                    global_aggregates,
                    grouping: Grouping::from_fields(group_fields),
                    // A build that spilled is uniformly lightweight (map cleared);
                    // keep the surviving in-RAM map otherwise.
                    residency: if spiller.spilled() {
                        Residency::Spilled
                    } else {
                        Residency::InRam(group_summaries)
                    },
                })
            },
            projection,
            created_at: now,
            last_accessed: now,
            incremental_updates: 0,
            lifecycle: lifecycle_for(is_auto),
            core_filters_cache: None,
            maintenance_degraded: false,
        };

        tracing::info!(
            "TRACE[3] create DONE: name={}, has_aggregates={}, agg_specs_count={}, group_fields={:?}, index_count={}",
            name,
            meta.is_aggregate(),
            meta.aggregate
                .as_ref()
                .map_or(0, |a| a.aggregate_specs.len()),
            meta.group_fields(),
            index_count
        );

        let sort_dir = if sort_inverted { "DESC" } else { "ASC" };
        let msg = format!(
            "Ghost '{}' created: {} index entries from '{}' ordered by '{}' {}",
            name, index_count, source_lobe, order_by_field, sort_dir
        );
        tracing::info!("{msg}");

        // Persist metadata to dictionary, then insert into runtime map
        self.persist_meta(&meta, dictionary)?;
        self.insert_ghost(meta);

        if ghost_compact_was_enabled {
            ks.set_compaction_enabled(true);
            // Compact ghost keyspace to drain L0 SSTables from ingest_sorted.
            // Without this, read_topn's prefix_iter has to merge over many L0 tables.
            let _ = ks.major_compact();
        }

        Ok(msg)
    }

    /// Create multiple ghosts in a single scan of the spatial keyspace.
    /// Each record is deserialized once and evaluated against all ghost filters.
    /// N ghosts over the same lobe = 1 scan instead of N scans.
    /// Batch ghost creation. Same contract as [`Self::create`]:
    /// `spatial` is the engine's single spatial Tree. Each spec in
    /// `specs` becomes one ghost; all share the single lobe scan.
    pub fn create_batch(
        &self,
        spatial: &Tree,
        dictionary: &Tree,
        lobe_id: u16,
        source_lobe: &str,
        specs: Vec<GhostSpec>,
        field_dict: Option<&xyzdb_core::field_dict::FieldDict>,
    ) -> Result<Vec<String>> {
        let ks = self.ks()?;

        // Disable bg compaction on ghost keyspace during creation — prevents
        // cleanup_orphan_ssts from deleting SSTables before they're registered in Version.
        let ghost_compact_was_enabled = ks.compaction_enabled();
        ks.set_compaction_enabled(false);

        // Prepare ghost state for each spec
        struct GhostBuild {
            ghost_id: u16,
            name: String,
            core_filter: crate::ops::CoreFilterExpr,
            order_by_field: String,
            sort_inverted: bool,
            aggregate_specs: Vec<crate::aggregate_state::Metric>,
            group_fields: Vec<String>,
            filter: ast::FilterExpr,
            is_auto: bool,
            projection: Vec<String>,
            metric_order: Option<MetricOrder>,
            index_count: u64,
            global_agg: crate::aggregate_state::AggregateState,
            group_summaries:
                std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>,
            spiller: RollupSpiller,
        }

        let mut builds: Vec<GhostBuild> = Vec::with_capacity(specs.len());
        for spec in &specs {
            if self.contains_ghost(&spec.name) {
                return Err(XyzError::GhostExists(spec.name.clone()));
            }
            let ghost_id = self.alloc_id();
            let core_filter = crate::ops::to_core_expr(&spec.filter);
            builds.push(GhostBuild {
                ghost_id,
                name: spec.name.clone(),
                core_filter,
                order_by_field: spec.order_by_field.clone(),
                sort_inverted: spec.sort_inverted,
                aggregate_specs: spec.aggregate_specs.clone(),
                group_fields: spec.group_fields.clone(),
                filter: spec.filter.clone(),
                is_auto: spec.is_auto,
                projection: spec.projection.clone(),
                metric_order: spec.metric_order.clone(),
                index_count: 0,
                global_agg: crate::aggregate_state::AggregateState::default(),
                group_summaries: std::collections::BTreeMap::new(),
                spiller: RollupSpiller::new(ghost_id),
            });
        }

        // Scan the spatial tree for this lobe's buckets.
        let prefix = lobe_id.to_be_bytes();
        let mut scanned = 0u64;

        for entry in spatial
            .prefix_iter(&prefix)
            .map_err(|e| XyzError::Storage(e.to_string()))?
        {
            let key_bytes = entry.key;
            let val = entry.value;

            let record = match xyzdb_core::record::deserialize_record(&val, source_lobe, field_dict)
            {
                Ok(r) => r,
                Err(_) => continue,
            };

            scanned += 1;
            if scanned.is_multiple_of(5_000_000) {
                tracing::info!("  Ghost batch: scanned {}M records...", scanned / 1_000_000);
            }

            // Evaluate each ghost's filters against this record
            for build in &mut builds {
                if !crate::ops::matches_core_expr(&record, &build.core_filter) {
                    continue;
                }

                // Write ghost entry: sort_key → ghost value. The tiebreak
                // (spatial key per record, or group key per group) keeps
                // equal sort values from overwriting each other.
                let sort_value = get_sort_value(&record, &build.order_by_field);
                let tiebreak =
                    ghost_entry_tiebreak(&build.group_fields, &record.fields, &key_bytes);
                let sort_key = crate::sort_encoding::encode_sort_key(
                    build.ghost_id,
                    sort_value,
                    build.sort_inverted,
                    &tiebreak,
                );
                let entry_value = encode_ghost_value(&key_bytes, &record, &build.projection);
                ks.insert(&sort_key, &entry_value)
                    .map_err(|e| XyzError::Storage(format!("ghost batch insert: {e}")))?;

                build.index_count += 1;

                // Drain ghost keyspace memtable periodically to avoid OOM
                if build.index_count % 100_000 == 0
                    && ks.active_memtable_size() > ks.max_memtable_size()
                {
                    ks.seal_active();
                    ks.flush_sealed()
                        .map_err(|e| XyzError::Storage(format!("ghost flush: {e}")))?;
                }

                // Update aggregates (bounded map — see `create`).
                if !build.aggregate_specs.is_empty() {
                    build.global_agg.add(&record, &build.aggregate_specs);
                    if !build.group_fields.is_empty() {
                        let gk = crate::aggregate_state::extract_group_key(
                            &record.fields,
                            &build.group_fields,
                        );
                        build
                            .group_summaries
                            .entry(gk)
                            .or_default()
                            .add(&record, &build.aggregate_specs);
                        build
                            .spiller
                            .maybe_spill(&mut build.group_summaries, dictionary)?;
                    }
                }
            }
        } // for entry

        tracing::info!(
            "Ghost batch: scanned {}M records total",
            scanned / 1_000_000
        );

        // Finalize each ghost
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        let mut messages = Vec::new();

        for mut build in builds {
            build
                .spiller
                .finalize(&mut build.group_summaries, dictionary)?;
            if build.spiller.spilled() {
                tracing::info!(
                    "ghost '{}' is lightweight: group rollups on disk (> {} groups)",
                    build.name,
                    group_spill_limit()
                );
            }
            // Metric-ordered rollup (see `create`): emit before the summaries map
            // moves into `meta`.
            let order_emitted_at = match &build.metric_order {
                Some(order) => {
                    let in_ram = if build.spiller.spilled() {
                        None
                    } else {
                        Some(&build.group_summaries)
                    };
                    match crate::ghost::metric_order::emit_metric_order(
                        dictionary,
                        build.ghost_id,
                        &build.group_fields,
                        order,
                        in_ram,
                    ) {
                        Ok(true) => Some(now),
                        Ok(false) => {
                            tracing::warn!(
                                "ghost '{}': metric-order emit hit a tiebreak collision; \
                                 order marked stale",
                                build.name
                            );
                            None
                        }
                        Err(e) => {
                            tracing::warn!(
                                "ghost '{}': metric-order emit failed: {e}; order marked stale",
                                build.name
                            );
                            None
                        }
                    }
                }
                None => None,
            };
            let meta = GhostMeta {
                name: build.name.clone(),
                ghost_id: build.ghost_id,
                version: 2,
                lobe_id,
                source_lobe: source_lobe.to_string(),
                filter: build.filter,
                order_by_field: build.order_by_field.clone(),
                sort_inverted: build.sort_inverted,
                metric_order: build.metric_order,
                order_emitted_at,
                state: 1, // Ready
                index_count: build.index_count,
                aggregate: if build.aggregate_specs.is_empty() {
                    None
                } else {
                    Some(AggregateContent {
                        aggregate_specs: build.aggregate_specs,
                        global_aggregates: build.global_agg,
                        grouping: Grouping::from_fields(build.group_fields),
                        residency: if build.spiller.spilled() {
                            Residency::Spilled
                        } else {
                            Residency::InRam(build.group_summaries)
                        },
                    })
                },
                projection: build.projection,
                created_at: now,
                last_accessed: now,
                incremental_updates: 0,
                lifecycle: lifecycle_for(build.is_auto),
                core_filters_cache: None,
                maintenance_degraded: false,
            };

            let dir = if build.sort_inverted { "DESC" } else { "ASC" };
            let msg = format!(
                "Ghost '{}' created: {} index entries from '{}' ordered by '{}' {}",
                meta.name, meta.index_count, source_lobe, meta.order_by_field, dir,
            );
            tracing::info!("{msg}");

            self.persist_meta(&meta, dictionary)?;
            self.insert_ghost(meta);
            messages.push(msg);
        }

        // Re-enable bg compaction and compact ghost keyspace to drain L0
        if ghost_compact_was_enabled {
            ks.set_compaction_enabled(true);
            let _ = ks.major_compact();
        }

        Ok(messages)
    }
}
