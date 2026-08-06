// SPDX-License-Identifier: BUSL-1.1
use super::*;

impl GhostLobeManager {
    // ─── Persistence ────────────────────────────────────────────────────

    /// Persist ghost metadata to the dictionary keyspace.
    pub(super) fn persist_meta(&self, meta: &GhostMeta, dictionary: &Tree) -> Result<()> {
        let persisted = PersistedGhostMeta {
            name: meta.name.clone(),
            ghost_id: meta.ghost_id,
            version: meta.version,
            lobe_id: meta.lobe_id,
            source_lobe: meta.source_lobe.clone(),
            // Lifecycle flattened to the on-disk flat fields (unchanged format);
            // `load` rebuilds the GhostLifecycle enum from ghost_type + ttl.
            is_auto: meta.is_auto(),
            filter: filter_expr_to_persisted(&meta.filter),
            order_by_field: meta.order_by_field.clone(),
            sort_inverted: meta.sort_inverted,
            metric_order: meta.metric_order.clone(),
            order_emitted_at: meta.order_emitted_at,
            state: meta.state,
            index_count: meta.index_count,
            // Aggregate overlay flattened back to the on-disk layout (unchanged
            // format): a covering ghost persists empty specs / default global /
            // empty summaries, exactly what `load` reads back as `None`.
            aggregate_specs: meta.aggregate.as_ref().map_or_else(Vec::new, |a| {
                a.aggregate_specs.iter().map(metric_to_persisted).collect()
            }),
            global_aggregates: meta
                .aggregate
                .as_ref()
                .map(|a| a.global_aggregates.clone())
                .unwrap_or_default(),
            group_fields: meta.group_fields().to_vec(),
            group_summaries: match meta.aggregate.as_ref().map(|a| &a.residency) {
                Some(Residency::InRam(m)) => m.clone(),
                _ => std::collections::BTreeMap::new(),
            },
            spilled: matches!(
                meta.aggregate.as_ref().map(|a| &a.residency),
                Some(Residency::Spilled)
            ),
            projection: meta.projection.clone(),
            created_at: meta.created_at,
            last_accessed: meta.last_accessed,
            incremental_updates: meta.incremental_updates,
            ghost_type: meta.ghost_type(),
            ttl_seconds: meta.ttl_seconds(),
            daily_access_bitmap: meta.telemetry().map_or(0, |t| t.daily_access_bitmap),
            access_count_total: meta.telemetry().map_or(0, |t| t.access_count_total),
        };
        let payload = postcard::to_allocvec(&persisted)
            .map_err(|e| XyzError::Storage(format!("ghost meta serialize: {e}")))?;
        let mut bytes = Vec::with_capacity(3 + payload.len());
        bytes.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
        bytes.push(GHOST_META_FORMAT);
        bytes.extend_from_slice(&payload);
        let key = meta_dictionary_key(meta.ghost_id);
        dictionary
            .insert(&key, &bytes)
            .map_err(|e| XyzError::Storage(format!("ghost meta persist: {e}")))?;
        Ok(())
    }

    /// Delete ghost metadata from the dictionary keyspace.
    pub(super) fn delete_meta(ghost_id: u16, dictionary: &Tree) -> Result<()> {
        let key = meta_dictionary_key(ghost_id);
        dictionary
            .remove(&key)
            .map_err(|e| XyzError::Storage(format!("ghost meta delete: {e}")))?;
        Ok(())
    }

    /// Load all persisted ghosts from the dictionary keyspace at boot.
    /// Returns (lobe_id, ghost_name, index_count) for router registration.
    ///
    /// **Boot-time TTL check.** The persisted `last_accessed` is the
    /// timestamp of the last `persist_meta` call — typically the ghost's
    /// create-time, since `bump_access` doesn't re-persist
    /// (the hot-read path writing to dictionary would add 150+ fsyncs/s
    /// under concurrent load, with no durable benefit). So persisted
    /// `last_accessed` lags real activity, sometimes by the entire uptime
    /// window.
    ///
    /// Still, if `now - persisted.last_accessed > ttl`, we know the ghost
    /// has NOT been persisted-touched for longer than its TTL. The worst
    /// case is an in-memory-hot ghost whose last persist was a create at
    /// T0 and is still actively used at T0+48h when the engine restarts —
    /// we drop it wrongly. Trade-off: the pattern re-triggers it via the
    /// auto-ghost threshold within minutes (5 hits in the 10min window at ≥20ms
    /// avg latency). Minutes of fallback to Primary in exchange for not
    /// accumulating orphan ghosts across reboots. Acceptable for v0.2.
    ///
    /// Without this check, an Ephemeral created-but-never-used survives
    /// indefinitely: every reboot gives it a fresh 24h lease via the
    /// boot-reset. The leak is small per ghost but monotonic — this
    /// boot-time TTL check is what keeps the ghost keyspace from growing
    /// unbounded.
    pub fn load_all(&mut self, dictionary: &Tree) -> Result<Vec<(u16, String, u64)>> {
        let mut restored = Vec::new();
        let mut max_id: u16 = self.next_id.load(Ordering::Relaxed);

        for entry in dictionary
            .prefix(&META_PREFIX)
            .map_err(|e| XyzError::Storage(e.to_string()))?
        {
            let persisted = match decode_persisted_ghost_meta(&entry.value) {
                DecodedMeta::Ok(p) => p,
                DecodedMeta::UnknownFormat { found } => {
                    tracing::warn!(
                        "Skipping ghost metadata with unsupported format byte 0x{found:02X} \
                         (this build writes 0x{current:02X}). The data dir was written by a \
                         build with a different ghost-metadata schema. Recreate the affected \
                         ghosts with `CREATE GHOST`; no record data is involved.",
                        current = GHOST_META_FORMAT,
                    );
                    continue;
                }
                DecodedMeta::Corrupt(e) => {
                    tracing::warn!("Skipping corrupt ghost metadata: {e}");
                    continue;
                }
            };

            if persisted.ghost_id >= max_id {
                max_id = persisted.ghost_id + 1;
            }

            // Boot-time TTL check: drop ghosts whose persisted TTL has elapsed.
            // Permanent ghosts (ttl_seconds == None) skip this check —
            // they have no TTL by definition. Same saturating-arithmetic
            // pattern as `identify_expired_ghosts` so clock skew / the
            // `u64::MAX as i64` wrap can't trip a Permanent into "expired."
            //
            // If `purge_ghost_data` fails (disk error mid-boot), DON'T
            // abort load_all and DON'T skip this ghost: fall through to
            // the insert, load the ghost into memory, and let the TTL
            // reaper retry the drop within ~60s. A boot that always opens
            // cleanly beats a boot that stalls on transient I/O.
            if let Some(ttl_s) = persisted.ttl_seconds {
                let now = now_micros();
                let elapsed = now.saturating_sub(persisted.last_accessed).max(0);
                let ttl_micros = (ttl_s as i64).saturating_mul(1_000_000);
                if elapsed > ttl_micros {
                    match self.purge_ghost_data(persisted.ghost_id, dictionary) {
                        Ok(_) => {
                            tracing::info!(
                                "boot: ghost '{}' (id={}) dropped — \
                                 persisted last_accessed > TTL ({}s)",
                                persisted.name,
                                persisted.ghost_id,
                                ttl_s
                            );
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "boot: failed to purge expired ghost '{}': {e}. \
                                 Loading into memory; TTL reaper will retry within ~60s.",
                                persisted.name
                            );
                            // Fall through to insert; reaper will re-drop.
                        }
                    }
                }
            }

            // Tracking fields (last_accessed, daily_access_bitmap,
            // access_count_total) are in-memory only by design —
            // `bump_access` never re-persists, so whatever's on disk is
            // stale the moment it gets there. Reset them at boot for every
            // ghost regardless of `ghost_type`:
            //
            //   - Ephemeral: the 24h TTL runs from this reboot, not from
            //     the last access pre-crash. A ghost that was about to
            //     expire lives 24h longer — acceptable by design.
            //   - Promoted: the 7-day consecutive-access bitmap restarts.
            //     The promotion detection requires 7 live days.
            //   - Permanent: `ttl_seconds == None`, so the reaper ignores
            //     it; the reset is a functional no-op but keeps the code
            //     path single.
            //
            // `ghost_type` and `ttl_seconds` ARE loaded from disk unchanged —
            // that's what tells the reaper how to treat this ghost.
            let meta = GhostMeta {
                name: persisted.name.clone(),
                ghost_id: persisted.ghost_id,
                version: persisted.version,
                lobe_id: persisted.lobe_id,
                source_lobe: persisted.source_lobe,
                filter: persisted_to_filter_expr(&persisted.filter),
                order_by_field: persisted.order_by_field,
                sort_inverted: persisted.sort_inverted,
                metric_order: persisted.metric_order,
                order_emitted_at: persisted.order_emitted_at,
                state: persisted.state,
                index_count: persisted.index_count,
                // Rebuild the overlay from the flat on-disk layout: empty specs
                // means a covering ghost (`None`); otherwise reassemble the
                // aggregate state. Mirrors `persist_meta`'s flattening.
                aggregate: if persisted.aggregate_specs.is_empty() {
                    None
                } else {
                    Some(AggregateContent {
                        aggregate_specs: persisted
                            .aggregate_specs
                            .iter()
                            .map(persisted_to_metric)
                            .collect(),
                        global_aggregates: persisted.global_aggregates,
                        grouping: Grouping::from_fields(persisted.group_fields),
                        residency: if persisted.spilled {
                            Residency::Spilled
                        } else {
                            Residency::InRam(persisted.group_summaries)
                        },
                    })
                },
                projection: persisted.projection,
                created_at: persisted.created_at,
                last_accessed: now_micros(),
                incremental_updates: persisted.incremental_updates,
                // Rebuild the lifecycle enum from the flat ghost_type + ttl.
                // Telemetry (bitmap/count) resets to 0 at boot, exactly as the
                // old load did — the reaper re-accrues it from live access.
                lifecycle: match persisted.ghost_type {
                    GhostType::Permanent => GhostLifecycle::Declared,
                    GhostType::Ephemeral => GhostLifecycle::Auto {
                        class: AutoClass::Ephemeral,
                        ttl_seconds: persisted.ttl_seconds.unwrap_or(EPHEMERAL_TTL_SECONDS),
                        telemetry: AccessTelemetry::default(),
                    },
                    GhostType::Promoted => GhostLifecycle::Auto {
                        class: AutoClass::Promoted,
                        ttl_seconds: persisted.ttl_seconds.unwrap_or(EPHEMERAL_TTL_SECONDS),
                        telemetry: AccessTelemetry::default(),
                    },
                },
                core_filters_cache: None,
                maintenance_degraded: false,
            };

            tracing::info!(
                "TRACE[6] BOOT restore ghost: name={}, lobe_id={}, agg_specs_count={}, group_fields={:?}, index_count={}, state={}",
                meta.name,
                meta.lobe_id,
                meta.aggregate
                    .as_ref()
                    .map_or(0, |a| a.aggregate_specs.len()),
                meta.group_fields(),
                meta.index_count,
                meta.state
            );

            restored.push((meta.lobe_id, meta.name.clone(), meta.index_count));
            self.insert_ghost(meta);
        }

        // Ensure next_id is past all restored ghosts
        self.next_id.fetch_max(max_id, Ordering::Relaxed);

        Ok(restored)
    }

    // ─── total_writes persistence ────────────────────────────────────

    /// Persist total_writes for a lobe to dictionary.
    pub fn persist_total_writes(dictionary: &Tree, lobe_id: u16, total_writes: u64) -> Result<()> {
        let mut key = Vec::with_capacity(4);
        key.extend_from_slice(&WRITES_PREFIX);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        dictionary
            .insert(&key, &total_writes.to_be_bytes())
            .map_err(|e| XyzError::Storage(format!("persist total_writes: {e}")))?;
        Ok(())
    }

    /// Load total_writes for a lobe from dictionary.
    ///
    /// The last of the bloom-gated point reads the recovery-bloom defect can reach
    /// (`KNOWN-ISSUES.md`). Its cost is the mildest of the set — a false "absent"
    /// resets a promotion counter to zero, so a ghost that had earned auto-creation
    /// has to earn it again; no data is lost and no constraint breaks. It is closed
    /// anyway, because "cheap to close" beat "cheap to lose" and leaving one door of
    /// a class open is how the class survives.
    ///
    /// `recovered_from_wal` gates the confirmation for the same reason as the anchor
    /// checks: outside that window the bloom is trusted and this costs nothing. No
    /// counter here — unlike a duplicated anchor, a re-earned promotion counter is
    /// not evidence anyone needs to act on.
    pub fn load_total_writes(dictionary: &Tree, lobe_id: u16, recovered_from_wal: bool) -> u64 {
        let mut key = Vec::with_capacity(4);
        key.extend_from_slice(&WRITES_PREFIX);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        let raw = match dictionary.get(&key).ok().flatten() {
            Some(v) => Some(v),
            None if recovered_from_wal => dictionary.get_no_bloom(&key).ok().flatten(),
            None => None,
        };
        raw.and_then(|v: Vec<u8>| {
            if v.len() == 8 {
                Some(u64::from_be_bytes(v[..8].try_into().unwrap()))
            } else {
                None
            }
        })
        .unwrap_or(0)
    }
}
