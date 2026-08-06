// SPDX-License-Identifier: BUSL-1.1
use super::*;

// ─── Ghost post-write hook ───────────────────────────────────────────────

impl GhostLobeManager {
    /// Append a signed rollup delta for one group (lightweight incremental
    /// path). A blind insert — no read, no lock-relevant RMW — relying on the
    /// rollup merge operator to fold the chain at compaction and read. A net
    /// count of 0 is filtered on read (`read_rollups`), so deletes need no
    /// remove. Errors degrade to a warn: notify_write is a post-commit hook
    /// and must never fail the user's write.
    /// Append a rollup delta. Returns `false` if the write failed (the caller
    /// marks the ghost maintenance-degraded so the failure is visible).
    fn rollup_append(
        dictionary: &Arc<Tree>,
        ghost_id: u16,
        ghost_name: &str,
        group_key: &str,
        delta: &crate::aggregate_state::RollupDelta,
    ) -> bool {
        let key = rollup_key(ghost_id, group_key);
        if let Err(e) = dictionary.insert(&key, &delta.encode()) {
            tracing::warn!("ghost '{ghost_name}' rollup append failed: {e}");
            return false;
        }
        true
    }
    /// Notify all ghosts of a write to the primary lobe.
    /// Called after the write has been committed to spatial/identity.
    /// Uses `&self` (not `&mut self`) — acquires write lock on ghosts internally.
    pub fn notify_write(
        &self,
        lobe_id: u16,
        record: &Record,
        spatial_key_bytes: &[u8],
        write_type: WriteType,
    ) {
        // Per-lobe fast path: lock ONLY this lobe's shard. A write to lobe A
        // never acquires lobe B's lock (TANDA B decoupling, by construction),
        // and if this lobe has no ghosts we return without touching any other
        // lobe's state. Replaces the old global write lock + `lobe_id` filter
        // over every ghost in the database.
        let Some(shard) = self.lobe_shard(lobe_id) else {
            return;
        };

        let start = std::time::Instant::now();

        // BULKMODE: aggregate maintenance is deferred to the post-load
        // REFRESH (see the `bulk_mode` field docs). Entry inserts proceed.
        let skip_aggregates = self.bulk_mode.load(Ordering::Relaxed);

        // Convert ghost AST filters to core filters once per call
        let mut ghosts = shard.write();
        for meta in ghosts.values_mut() {
            if meta.state == 2 {
                continue; // Paused
            }

            // Core-filter form is immutable per ghost (filters never change
            // after creation), so build it once and memoise it on the meta
            // instead of reconverting + deep-cloning every literal on every
            // write (audit P2-2). Filled under the write lock we already hold.
            meta.ensure_core_filters();

            match &write_type {
                WriteType::Insert => {
                    // Resolve the match first so the borrow of the cached
                    // filters ends before `meta` is passed mutably below.
                    let matched = crate::ops::matches_core_expr(
                        record,
                        meta.core_filters_cache.as_ref().unwrap(),
                    );
                    if matched {
                        Self::ghost_insert_inner(
                            self.keyspace.as_ref(),
                            self.dictionary.as_ref(),
                            meta,
                            record,
                            spatial_key_bytes,
                            skip_aggregates,
                        );
                    }
                }
                WriteType::Delete => {
                    let matched = crate::ops::matches_core_expr(
                        record,
                        meta.core_filters_cache.as_ref().unwrap(),
                    );
                    if matched {
                        Self::ghost_remove_inner(
                            self.keyspace.as_ref(),
                            self.dictionary.as_ref(),
                            meta,
                            record,
                            spatial_key_bytes,
                            skip_aggregates,
                        );
                    }
                }
                WriteType::Update {
                    old_record,
                    old_spatial_key,
                } => {
                    let (old_matches, new_matches) = {
                        let core_filters = meta.core_filters_cache.as_ref().unwrap();
                        (
                            crate::ops::matches_core_expr(old_record, core_filters),
                            crate::ops::matches_core_expr(record, core_filters),
                        )
                    };

                    match (old_matches, new_matches) {
                        (false, false) => {} // irrelevant
                        (false, true) => {
                            // New record enters the ghost — key it by the NEW
                            // spatial key (where the record now lives).
                            Self::ghost_insert_inner(
                                self.keyspace.as_ref(),
                                self.dictionary.as_ref(),
                                meta,
                                record,
                                spatial_key_bytes,
                                skip_aggregates,
                            );
                        }
                        (true, false) => {
                            // Record exits the ghost — remove the entry keyed by
                            // the OLD spatial key (where it was inserted). A
                            // re-gravitating SET moved the record; the new key
                            // never had an entry.
                            Self::ghost_remove_inner(
                                self.keyspace.as_ref(),
                                self.dictionary.as_ref(),
                                meta,
                                old_record,
                                old_spatial_key,
                                skip_aggregates,
                            );
                        }
                        (true, true) => {
                            // Record stays in the ghost — move its entry from the
                            // old key to the new one (both may differ under a
                            // re-gravitating SET) and refresh aggregates.
                            Self::ghost_update_inner(
                                self.keyspace.as_ref(),
                                self.dictionary.as_ref(),
                                meta,
                                old_record,
                                record,
                                old_spatial_key,
                                spatial_key_bytes,
                                skip_aggregates,
                            );
                        }
                    }
                }
            }

            meta.incremental_updates += 1;
        }

        // Trigger ghost keyspace flush if memtable is full.
        // Without this, ghost inserts via notify_write accumulate in the memtable
        // indefinitely because they bypass the WriteBatch path that calls maybe_trigger_flush.
        if let Some(ks) = self.keyspace.as_ref()
            && ks.active_memtable_size() >= ks.max_memtable_size()
        {
            ks.seal_active();
            ks.notify_bg();
        }
        // Same for the dictionary keyspace: lightweight rollup RMWs go
        // through bare Tree::insert and need the manual flush trigger.
        if let Some(dict) = self.dictionary.as_ref()
            && dict.active_memtable_size() >= dict.max_memtable_size()
        {
            dict.seal_active();
            dict.notify_bg();
        }

        let elapsed_ns = start.elapsed().as_nanos() as u64;
        self.overhead_tracker.update(elapsed_ns);
    }

    /// Insert a record into a ghost's index and update aggregates.
    fn ghost_insert_inner(
        keyspace: Option<&Arc<Tree>>,
        dictionary: Option<&Arc<Tree>>,
        meta: &mut GhostMeta,
        record: &Record,
        spatial_key_bytes: &[u8],
        skip_aggregates: bool,
    ) {
        let Some(ks) = keyspace else { return };

        let sort_value = get_sort_value(record, &meta.order_by_field);
        let tiebreak = ghost_entry_tiebreak(meta.group_fields(), &record.fields, spatial_key_bytes);
        let sort_key = crate::sort_encoding::encode_sort_key(
            meta.ghost_id,
            sort_value,
            meta.sort_inverted,
            &tiebreak,
        );
        let entry_value = encode_ghost_value(spatial_key_bytes, record, &meta.projection);
        if let Err(e) = ks.insert(&sort_key, &entry_value) {
            tracing::warn!("ghost '{}' insert failed: {}", meta.name, e);
            meta.maintenance_degraded = true;
            return;
        }

        meta.index_count += 1;

        if !skip_aggregates && let Some(agg) = meta.aggregate.as_mut() {
            agg.global_aggregates.add(record, &agg.aggregate_specs);

            if agg.grouping.is_grouped() {
                let gk = crate::aggregate_state::extract_group_key(
                    &record.fields,
                    agg.grouping.fields(),
                );
                if matches!(agg.residency, Residency::Spilled) {
                    // Lightweight: append a +1 delta for the group (no RMW).
                    if let Some(dict) = dictionary {
                        let delta = crate::aggregate_state::RollupDelta::for_record(
                            record,
                            &agg.aggregate_specs,
                            1,
                        );
                        if !Self::rollup_append(dict, meta.ghost_id, &meta.name, &gk, &delta) {
                            meta.maintenance_degraded = true;
                        }
                    }
                    return;
                }
                let mut spilled_now = false;
                if let Residency::InRam(map) = &mut agg.residency {
                    map.entry(gk).or_default().add(record, &agg.aggregate_specs);
                    // Incremental growth past the in-RAM budget flips the ghost
                    // to lightweight: spill everything, clear, and mark Spilled
                    // so the residency state is explicit from here on.
                    if map.len() > group_spill_limit()
                        && let Some(dict) = dictionary
                    {
                        let mut spiller = RollupSpiller::new(meta.ghost_id);
                        match spiller.spill(map, dict) {
                            Ok(()) => {
                                spilled_now = true;
                                tracing::info!(
                                    "ghost '{}' is lightweight: group rollups on disk (> {} groups)",
                                    meta.name,
                                    group_spill_limit()
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "ghost '{}' incremental spill failed: {e}",
                                    meta.name
                                )
                            }
                        }
                    }
                }
                if spilled_now {
                    agg.residency = Residency::Spilled;
                }
            }
        }
    }

    /// Remove a record from a ghost's index and subtract aggregates.
    fn ghost_remove_inner(
        keyspace: Option<&Arc<Tree>>,
        dictionary: Option<&Arc<Tree>>,
        meta: &mut GhostMeta,
        record: &Record,
        spatial_key_bytes: &[u8],
        skip_aggregates: bool,
    ) {
        let Some(ks) = keyspace else { return };

        let sort_value = get_sort_value(record, &meta.order_by_field);
        // Same (sort_value, tiebreak, inverted) the entry was inserted with, so
        // the exact entry is removed rather than a colliding sibling. For a
        // grouped ghost the tiebreak is the group key, so a remove targets the
        // group's entry (records in a group share it; the aggregate subtract
        // tracks the count).
        let tiebreak = ghost_entry_tiebreak(meta.group_fields(), &record.fields, spatial_key_bytes);
        let sort_key = crate::sort_encoding::encode_sort_key(
            meta.ghost_id,
            sort_value,
            meta.sort_inverted,
            &tiebreak,
        );
        if let Err(e) = ks.remove(&sort_key) {
            tracing::warn!("ghost '{}' remove failed: {}", meta.name, e);
            meta.maintenance_degraded = true;
            return;
        }

        meta.index_count = meta.index_count.saturating_sub(1);

        if !skip_aggregates && let Some(agg) = meta.aggregate.as_mut() {
            agg.global_aggregates.subtract(record, &agg.aggregate_specs);

            if agg.grouping.is_grouped() {
                let gk = crate::aggregate_state::extract_group_key(
                    &record.fields,
                    agg.grouping.fields(),
                );
                if matches!(agg.residency, Residency::Spilled) {
                    // Lightweight: append a -1 delta (count 0 filtered on read).
                    if let Some(dict) = dictionary {
                        let delta = crate::aggregate_state::RollupDelta::for_record(
                            record,
                            &agg.aggregate_specs,
                            -1,
                        );
                        if !Self::rollup_append(dict, meta.ghost_id, &meta.name, &gk, &delta) {
                            meta.maintenance_degraded = true;
                        }
                    }
                    return;
                }
                if let Residency::InRam(map) = &mut agg.residency
                    && let Some(group_state) = map.get_mut(&gk)
                {
                    group_state.subtract(record, &agg.aggregate_specs);
                    if group_state.count == 0 {
                        map.remove(&gk);
                    }
                }
            }
        }
    }

    /// Update a record's ghost entry, moving it if its key changed, and refresh
    /// aggregates.
    ///
    /// The entry key is `(sort_value, tiebreak(spatial_key))`. A SET can change
    /// EITHER component: the sort value (ORDER BY field edited) or the spatial
    /// key (a re-gravitating SET — on a no-anchor lobe every SET moves it, since
    /// the gravity hash falls back to hashing all fields). So the old entry must
    /// be located with the OLD spatial key and the new one written under the NEW
    /// spatial key; using a single key for both dangles the entry whenever the
    /// record re-gravitated (silent covering loss, `index_count` unaware).
    #[allow(clippy::too_many_arguments)] // old + new spatial key must both be threaded
    fn ghost_update_inner(
        keyspace: Option<&Arc<Tree>>,
        dictionary: Option<&Arc<Tree>>,
        meta: &mut GhostMeta,
        old_record: &Record,
        new_record: &Record,
        old_spatial_key: &[u8],
        new_spatial_key: &[u8],
        skip_aggregates: bool,
    ) {
        let Some(ks) = keyspace else { return };

        let old_sort_key = crate::sort_encoding::encode_sort_key(
            meta.ghost_id,
            get_sort_value(old_record, &meta.order_by_field),
            meta.sort_inverted,
            &ghost_entry_tiebreak(meta.group_fields(), &old_record.fields, old_spatial_key),
        );
        let new_sort_key = crate::sort_encoding::encode_sort_key(
            meta.ghost_id,
            get_sort_value(new_record, &meta.order_by_field),
            meta.sort_inverted,
            &ghost_entry_tiebreak(meta.group_fields(), &new_record.fields, new_spatial_key),
        );
        // Store the full ghost value (spatial key + projection), matching
        // ghost_insert_inner, so a projected ghost's fast-path read and a
        // covering ghost's point-read both resolve after the update.
        let entry_value = encode_ghost_value(new_spatial_key, new_record, &meta.projection);
        if new_sort_key != old_sort_key {
            // Insert the new entry BEFORE removing the old one: never a window
            // where the record is absent from the ghost. A transient duplicate
            // (both keys point-read the same record) is harmless; a transient
            // gap would reproduce the very loss this fixes, from the other side.
            if let Err(e) = ks.insert(&new_sort_key, &entry_value) {
                tracing::warn!("ghost '{}' update insert failed: {}", meta.name, e);
                meta.maintenance_degraded = true;
            }
            if let Err(e) = ks.remove(&old_sort_key) {
                tracing::warn!("ghost '{}' update remove failed: {}", meta.name, e);
                meta.maintenance_degraded = true;
            }
        } else {
            // Same key: overwrite in place so the stored value (projection /
            // spatial-key reference) stays fresh after the SET.
            if let Err(e) = ks.insert(&new_sort_key, &entry_value) {
                tracing::warn!("ghost '{}' update insert failed: {}", meta.name, e);
                meta.maintenance_degraded = true;
            }
        }

        // Update aggregates: subtract old, add new
        if !skip_aggregates && let Some(agg) = meta.aggregate.as_mut() {
            agg.global_aggregates
                .subtract(old_record, &agg.aggregate_specs);
            agg.global_aggregates.add(new_record, &agg.aggregate_specs);

            if agg.grouping.is_grouped() {
                let old_gk = crate::aggregate_state::extract_group_key(
                    &old_record.fields,
                    agg.grouping.fields(),
                );
                let new_gk = crate::aggregate_state::extract_group_key(
                    &new_record.fields,
                    agg.grouping.fields(),
                );

                if matches!(agg.residency, Residency::Spilled) {
                    // Lightweight: append deltas (no RMW). Same group folds
                    // -old +new into one delta; a group move appends one delta
                    // per side.
                    if let Some(dict) = dictionary {
                        use crate::aggregate_state::RollupDelta;
                        if old_gk == new_gk {
                            let mut delta =
                                RollupDelta::for_record(new_record, &agg.aggregate_specs, 1);
                            delta.merge(&RollupDelta::for_record(
                                old_record,
                                &agg.aggregate_specs,
                                -1,
                            ));
                            if !Self::rollup_append(
                                dict,
                                meta.ghost_id,
                                &meta.name,
                                &old_gk,
                                &delta,
                            ) {
                                meta.maintenance_degraded = true;
                            }
                        } else {
                            let ok_old = Self::rollup_append(
                                dict,
                                meta.ghost_id,
                                &meta.name,
                                &old_gk,
                                &RollupDelta::for_record(old_record, &agg.aggregate_specs, -1),
                            );
                            let ok_new = Self::rollup_append(
                                dict,
                                meta.ghost_id,
                                &meta.name,
                                &new_gk,
                                &RollupDelta::for_record(new_record, &agg.aggregate_specs, 1),
                            );
                            if !ok_old || !ok_new {
                                meta.maintenance_degraded = true;
                            }
                        }
                    }
                } else if let Residency::InRam(map) = &mut agg.residency {
                    if old_gk == new_gk {
                        // Same group: subtract old, add new
                        if let Some(group_state) = map.get_mut(&old_gk) {
                            group_state.subtract(old_record, &agg.aggregate_specs);
                            group_state.add(new_record, &agg.aggregate_specs);
                        }
                    } else {
                        // Group changed: subtract from old group, add to new group
                        if let Some(old_state) = map.get_mut(&old_gk) {
                            old_state.subtract(old_record, &agg.aggregate_specs);
                            if old_state.count == 0 {
                                map.remove(&old_gk);
                            }
                        }
                        map.entry(new_gk)
                            .or_default()
                            .add(new_record, &agg.aggregate_specs);
                    }
                }
            }
        }
    }
}
