use super::*;

impl GhostLobeManager {
    /// Read TOP-N records from a ghost.
    ///
    /// Iterates the ghost keyspace in order (already sorted by sort_key).
    /// Each entry's value is a spatial key — does a point-read on spatial to get full record.
    /// Applies extra_filters if provided.
    ///
    /// `spatial` is the engine's single spatial Tree. The
    /// fallback path does a point-read on it per entry whose
    /// spatial key needs the full record; the projection fast
    /// path never touches it.
    ///
    /// `vectors` is the V5 vector-column Tree, keyed by the same spatial key.
    /// The fallback path hydrates from it so a ghost-routed read returns the
    /// same fields as the identical query answered from the primary keyspace.
    pub fn read_topn(
        &self,
        name: &str,
        n: usize,
        extra_filters: &[Filter],
        spatial: &Tree,
        vectors: &Tree,
        field_dict: Option<&xyzdb_core::field_dict::FieldDict>,
    ) -> Result<Vec<Record>> {
        let shard = self
            .shard_for_name(name)
            .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))?;
        let ghosts = shard.read();
        let meta = ghosts
            .get(name)
            .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))?;

        let ks = self.ks()?;
        let ghost_prefix = meta.ghost_id.to_be_bytes();
        let source_lobe = meta.source_lobe.clone();

        let core_filters: Vec<(String, FilterOp, Value)> = extra_filters
            .iter()
            .map(|f| {
                (
                    f.field.clone(),
                    crate::ops::convert_filter_op(&f.op),
                    crate::ops::literal_to_value(&f.value),
                )
            })
            .collect();

        let has_projection = meta.has_projection();
        let projection = &meta.projection;
        let spatial_key_len = xyzdb_core::key::SPATIAL_KEY_SIZE; // Reads the current layout — v0.5.x=18, v0.6.0-pre=22.

        let mut results = Vec::with_capacity(n.min(1024));

        // Ghost-seek: narrow the scan when the query constrains the ghost's
        // ordered field (covering ghosts only). Eq → prefix seek; range
        // (>,>=,<,<=, incl. desugared BETWEEN) → byte-range seek so prefix/range
        // iter binary-searches to the matching block instead of scanning every
        // entry. The bounds are a CONSERVATIVE I/O window — the loop below still
        // applies every filter, so strict-vs-inclusive and DESC stay exact.
        // Reuses the insert-side encode_sort_key (empty tiebreak); the prefix-free
        // encoding keeps the window correct. A query with no ordered-field
        // predicate keeps the full [ghost_id] prefix (zero regression). Matches
        // `matches_filters` Eq, which is variant-strict `==` (no Int/Float
        // coercion), exactly mirrored by the per-type encoding tag.
        let mut eq_val: Option<Value> = None;
        let mut lo_val: Option<Value> = None; // from Gt/Gte
        let mut hi_val: Option<Value> = None; // from Lt/Lte
        if !meta.is_grouped() {
            for (field, op, val) in &core_filters {
                if field != &meta.order_by_field {
                    continue;
                }
                match op {
                    FilterOp::Eq => eq_val = Some(val.clone()),
                    FilterOp::Gt | FilterOp::Gte => lo_val = Some(val.clone()),
                    FilterOp::Lt | FilterOp::Lte => hi_val = Some(val.clone()),
                    _ => {}
                }
            }
        }
        let gid = meta.ghost_id;
        let inv = meta.sort_inverted;
        let key_of = |v: &Value| crate::sort_encoding::encode_sort_key(gid, Some(v), inv, &[]);

        let scan_iter = if let Some(v) = &eq_val {
            ks.prefix_iter(&key_of(v))
                .map_err(|e| XyzError::Storage(e.to_string()))?
        } else if lo_val.is_some() || hi_val.is_some() {
            // Byte window [start, end) covering the value range; under inversion
            // the value→byte order flips, so lo/hi swap. Open sides fall back to
            // the ghost extent ([ghost_id] .. [ghost_id+1]); `None` end = scan to
            // the keyspace tail (only when this is the highest ghost_id).
            let ghost_hi = byte_successor(&ghost_prefix);
            let (start, end): (Vec<u8>, Option<Vec<u8>>) = if !inv {
                let s = lo_val
                    .as_ref()
                    .map(&key_of)
                    .unwrap_or_else(|| ghost_prefix.to_vec());
                let e = hi_val
                    .as_ref()
                    .map(|v| byte_successor(&key_of(v)))
                    .unwrap_or(ghost_hi);
                (s, e)
            } else {
                let s = hi_val
                    .as_ref()
                    .map(&key_of)
                    .unwrap_or_else(|| ghost_prefix.to_vec());
                let e = lo_val
                    .as_ref()
                    .map(|v| byte_successor(&key_of(v)))
                    .unwrap_or(ghost_hi);
                (s, e)
            };
            ks.range_iter(&start, end.as_deref())
                .map_err(|e| XyzError::Storage(e.to_string()))?
        } else {
            ks.prefix_iter(&ghost_prefix)
                .map_err(|e| XyzError::Storage(e.to_string()))?
        };

        // Streaming scan: only reads entries until N are found.
        for entry in scan_iter {
            if results.len() >= n {
                break;
            }

            // Fast path when ghost has embedded projection:
            // Ghost entries already match the ghost's filters, so extra_filters
            // (which are the query's WHERE clause) are redundant — the ghost was
            // created with those same filters. Skip point reads entirely.
            let record = if has_projection {
                // Fast path: decode projected fields directly from ghost entry.
                // No point read to spatial keyspace.
                match decode_ghost_projection(
                    &entry.value,
                    spatial_key_len,
                    projection,
                    &source_lobe,
                ) {
                    Some(r) => r,
                    None => continue,
                }
            } else {
                // Fallback: point-read from spatial (no
                // projection, or extra filters need full record).
                let spatial_key_bytes = &entry.value[..spatial_key_len.min(entry.value.len())];
                if spatial_key_bytes.len() < xyzdb_core::key::SPATIAL_KEY_SIZE {
                    // Truncated key — skip rather than panic.
                    continue;
                }
                let record_bytes = match spatial.get(spatial_key_bytes) {
                    Ok(Some(v)) => v,
                    _ => continue,
                };
                // Hydrate the V5 vector column, exactly as the primary read
                // paths do. This path returns the FULL record, so a missing
                // declared vector is not a projection — it is the same query
                // answering differently depending on whether a ghost happens to
                // exist, which is the one thing a ghost must never do.
                match crate::ops::deserialize_hydrated_with(
                    vectors,
                    spatial_key_bytes,
                    &record_bytes,
                    &source_lobe,
                    field_dict,
                ) {
                    Ok(r) => r,
                    Err(_) => continue,
                }
            };

            // Apply extra filters only on the fallback path (full record).
            // Projected records don't have all fields — the ghost's own filters
            // already guarantee the match, so skip redundant filter checks.
            if !has_projection && !core_filters.is_empty() && !record.matches_filters(&core_filters)
            {
                continue;
            }

            results.push(record);
        }

        Ok(results)
    }

    /// Return pre-computed aggregates from ghost metadata.
    ///
    /// - If `group_by` is requested and matches the ghost's group_fields:
    ///   returns group_summaries, filtered by `query_filters` on
    ///   group-key fields (see below).
    /// - If no `group_by`: returns the global_aggregates.
    /// - Zero scan, microsecond response.
    ///
    /// # Filter contract (Finding 11 — v0.2.4)
    ///
    /// `query_filters` are the query's `WHERE` predicates in the form
    /// `(field, FilterOp, Value)`. This method applies only those
    /// predicates that satisfy **both**:
    ///
    /// 1. `field` is in the ghost's `group_fields`, **and**
    /// 2. the operator is `FilterOp::Eq`.
    ///
    /// Other predicates are silently ignored on the assumption that the
    /// caller (`ghost_router::plan_scan`) has already rejected any
    /// query whose predicates fall outside the supported shape — such
    /// queries must route to Primary instead, never to PreComputed.
    ///
    /// This is the minimum sufficient scope for v0.2.4. Non-`Eq`
    /// operators on group keys (`!=`, `<`, `<=`, `>`, `>=`, `IN`, …)
    /// are not yet implemented here; the router must steer such
    /// queries to Primary. Incremental operator support lands in v0.3+.
    pub fn read_precomputed(
        &self,
        name: &str,
        group_by: &[String],
        query_filters: &[(String, FilterOp, Value)],
    ) -> Result<GhostAggregates> {
        let shard = self
            .shard_for_name(name)
            .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))?;
        let ghosts = shard.read();
        let meta = ghosts
            .get(name)
            .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))?;

        // The router only routes aggregate ghosts here; a covering ghost has no
        // precomputed state to return.
        let Some(agg) = meta.aggregate.as_ref() else {
            return Ok(GhostAggregates::Global(Default::default()));
        };

        if !group_by.is_empty() && group_by == meta.group_fields() {
            // Build the expected key fragment for each group field:
            // Some(expected_fragment) when the query carries an Eq
            // predicate on that field, None (wildcard) otherwise.
            // Must match the encoding produced by
            // `aggregate_state::extract_group_key` (single source of
            // truth via `value_to_group_key_fragment`).
            let expected_parts: Vec<Option<String>> = meta
                .group_fields()
                .iter()
                .map(|gf| {
                    query_filters
                        .iter()
                        .find(|(f, op, _)| f == gf && *op == FilterOp::Eq)
                        .map(|(_, _, v)| {
                            crate::aggregate_state::value_to_group_key_fragment(Some(v))
                        })
                })
                .collect();

            let all_wildcard = expected_parts.iter().all(|p| p.is_none());

            // Lightweight ghost (group rollups on disk): an empty in-RAM
            // map with declared group fields means the groups live in the
            // rollup namespace — by spill (high cardinality) or trivially
            // (no groups yet; the empty range reads the same). Capture the
            // ghost_id and release the registry lock before disk I/O.
            if matches!(agg.residency, Residency::Spilled) {
                let ghost_id = meta.ghost_id;
                drop(ghosts);
                return Ok(GhostAggregates::Grouped(self.read_rollups(
                    ghost_id,
                    &expected_parts,
                    all_wildcard,
                )?));
            }

            // In-RAM from here (Spilled returned above); a grouped ghost always
            // carries an InRam map at this point. An empty map reads as no groups,
            // exactly as the old empty-map path did.
            let Residency::InRam(map) = &agg.residency else {
                return Ok(GhostAggregates::Grouped(std::collections::BTreeMap::new()));
            };
            let filtered: std::collections::BTreeMap<
                String,
                crate::aggregate_state::AggregateState,
            > = if all_wildcard {
                map.clone()
            } else if expected_parts.iter().all(|p| p.is_some()) {
                // Every group field is pinned by an Eq predicate, so the group
                // key is fully determined: an O(log N) BTreeMap point lookup
                // instead of an O(N) scan over every group. Critical at scale —
                // `credits_by_rfc` holds one group per rfc (millions of them),
                // so the linear filter made a single-rfc Q2 walk all of them
                // (the ~335 ms @ scale-1 cost). Same length-prefixed encoding as
                // `extract_group_key` — every part is pinned in this branch.
                let pinned: Vec<String> = expected_parts
                    .iter()
                    .map(|p| p.clone().unwrap_or_default())
                    .collect();
                let key = crate::aggregate_state::encode_group_key(&pinned);
                match map.get(&key) {
                    Some(state) => std::iter::once((key, state.clone())).collect(),
                    None => std::collections::BTreeMap::new(),
                }
            } else {
                map.iter()
                    .filter(|(gk, _)| {
                        let parts = crate::aggregate_state::decode_group_key(gk);
                        expected_parts.iter().enumerate().all(|(i, ep)| match ep {
                            Some(expected) => parts.get(i).map(|p| p == expected).unwrap_or(false),
                            None => true,
                        })
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };

            Ok(GhostAggregates::Grouped(filtered))
        } else {
            // Return global aggregates. Query-filter scope for the
            // Global branch is enforced upstream by the router: if the
            // query carries any predicate that is not a ghost constant,
            // the router routes to Primary instead of PreComputed.
            Ok(GhostAggregates::Global(agg.global_aggregates.clone()))
        }
    }

    /// Read group rollups of a lightweight ghost from the dictionary
    /// keyspace. One canonical entry per group.
    ///
    /// - Fully pinned (every group field has an Eq) → ONE exact `get`:
    ///   bloom-filtered, block-cached — the on-disk analogue of the
    ///   in-RAM point lookup. (The first cut range-scanned partials here;
    ///   at 10M+ rollup entries that cost ~20 ms per lookup.)
    /// - Wildcard / partial pin → scan of the ghost's rollup range with
    ///   the same fragment filter the in-RAM path applies. Expensive at
    ///   high cardinality by nature — matches the cost the equivalent
    ///   in-RAM clone already paid.
    fn read_rollups(
        &self,
        ghost_id: u16,
        expected_parts: &[Option<String>],
        all_wildcard: bool,
    ) -> Result<std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>> {
        let mut out: std::collections::BTreeMap<String, crate::aggregate_state::AggregateState> =
            std::collections::BTreeMap::new();
        let Some(dict) = self.dictionary.as_ref() else {
            return Ok(out);
        };

        // Scan prefix: the exact group key when fully pinned (matches that one
        // group via the klen-prefixed encoding), else the whole ghost. The
        // dictionary tree's rollup merge operator folds each group's delta
        // chain on read, so prefix_iter yields one folded RollupDelta per group
        // — hence a SCAN, not a point `get` (which would see only the newest
        // delta; point reads are not merge-folded by design).
        let prefix = if !all_wildcard && expected_parts.iter().all(|p| p.is_some()) {
            let pinned: Vec<String> = expected_parts
                .iter()
                .map(|p| p.clone().unwrap_or_default())
                .collect();
            let key = crate::aggregate_state::encode_group_key(&pinned);
            rollup_key(ghost_id, &key)
        } else {
            rollup_ghost_prefix(ghost_id)
        };

        for entry in dict
            .prefix_iter(&prefix)
            .map_err(|e| XyzError::Storage(format!("rollup scan: {e}")))?
        {
            let Some(gk) = rollup_key_group(&entry.key) else {
                continue;
            };
            if !all_wildcard {
                let parts = crate::aggregate_state::decode_group_key(gk);
                let keep = expected_parts.iter().enumerate().all(|(i, ep)| match ep {
                    Some(expected) => parts.get(i).map(|p| p == expected).unwrap_or(false),
                    None => true,
                });
                if !keep {
                    continue;
                }
            }
            if let Some(d) = crate::aggregate_state::decode_rollup_delta(&entry.value) {
                out.insert(gk.to_string(), d.into_aggregate_state());
            }
        }
        // Groups whose folded delta nets to count 0 are not groups.
        out.retain(|_, st| st.count > 0);
        Ok(out)
    }

    /// Serve the top-`n` groups of a ghost from its metric-ordered rollup — O(N).
    /// Returns `None` (caller falls back to the O(M) path) when the ghost's
    /// declared `ORDER BY <metric>` does not match the requested (label,
    /// direction), when its group fields differ, or when the order is stale
    /// (never emitted / last emit failed). Rows are byte-identical to
    /// `read_precomputed` + `apply_top` (shared row builder).
    pub fn read_topn_metric(
        &self,
        name: &str,
        group_fields: &[String],
        label: &str,
        descending: bool,
        n: usize,
    ) -> Result<Option<Vec<std::collections::BTreeMap<String, Value>>>> {
        let shard = self
            .shard_for_name(name)
            .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))?;
        let ghost_id = {
            let ghosts = shard.read();
            let meta = ghosts
                .get(name)
                .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))?;
            let matches = meta.order_emitted_at.is_some()
                && meta
                    .metric_order
                    .as_ref()
                    .is_some_and(|mo| mo.label == label && mo.descending == descending)
                && meta.group_fields() == group_fields;
            if !matches {
                return Ok(None);
            }
            meta.ghost_id
        };
        let Some(dict) = self.dictionary.as_ref() else {
            return Ok(None);
        };
        Ok(Some(metric_order::read_topn(
            dict,
            ghost_id,
            group_fields,
            n,
        )?))
    }
}
