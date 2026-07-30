use super::*;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Test-only knobs for MIGRATE crash coverage (durability gate). Both are
/// no-ops in production (`0` = off) and follow the `FORCE_*` injection pattern
/// used elsewhere for durability tests (e.g. `journal::writer::FORCE_WRITE_ENOSPC`).
///
/// - `MIGRATE_WINDOW_LIMIT` overrides the per-window commit size (default
///   10_000 records) so a crash test can cross a window boundary without a
///   10k-row dataset.
/// - `FORCE_MIGRATE_ABORT_AFTER_WINDOWS` makes [`Engine::execute_migrate`] return
///   an error immediately after committing this many windows — modelling a crash
///   mid-migration with the earlier windows already durable, so a re-run must
///   complete the rest idempotently (no loss, no key aliasing).
pub static MIGRATE_WINDOW_LIMIT: AtomicUsize = AtomicUsize::new(0);
pub static FORCE_MIGRATE_ABORT_AFTER_WINDOWS: AtomicU32 = AtomicU32::new(0);

impl Engine {
    /// LSM compaction pressure metrics for throttle evaluation.
    pub fn lsm_pressure(&self) -> (usize, usize) {
        let l0 = self.turba.spatial.l0_table_count();
        let sealed = self.turba.spatial.sealed_memtable_count();
        (l0, sealed)
    }

    // ── COMPACT ──────────────────────────────────────────────────────

    pub(super) fn execute_compact(&self) -> Result<QueryResult> {
        let start = std::time::Instant::now();

        tracing::info!("COMPACT: starting major compaction on all keyspaces");

        // Seal active memtables first so that tree.major_compact()'s
        // internal flush_sealed() picks them up. Without this, any
        // writes still in the active memtable are orphaned when
        // rotate_journal truncates the WAL below — Finding 8 path B
        // (the server COMPACT command goes here, not through
        // TurbaEngine::major_compact where the original fix landed).
        self.turba.spatial.seal_active();
        self.turba.identity.seal_active();
        self.turba.dictionary.seal_active();
        self.turba.vectors.seal_active();

        self.turba
            .spatial
            .major_compact()
            .map_err(|e| XyzError::Storage(format!("spatial compact: {e}")))?;
        self.turba
            .identity
            .major_compact()
            .map_err(|e| XyzError::Storage(format!("identity compact: {e}")))?;
        self.turba
            .dictionary
            .major_compact()
            .map_err(|e| XyzError::Storage(format!("dictionary compact: {e}")))?;
        self.turba
            .vectors
            .major_compact()
            .map_err(|e| XyzError::Storage(format!("vectors compact: {e}")))?;
        self.ghost_manager.flush()?;

        // All data is now in SSTables — every keyspace sealed + major-compacted
        // above (spatial/identity/dictionary/vectors; the dictionary also holds
        // the field-registry entries the LINK write path co-commits) plus
        // ghost_manager.flush() for the ghost lobe — so the WAL is safe to
        // truncate. The `vectors` keyspace is co-committed with `spatial` in one
        // batch (ops/put.rs), so its active memtable MUST be flushed here too;
        // omitting it dropped acked vectors on crash (the compact-skips-vectors
        // durability bug). rotate_journal now VERIFIES this precondition and
        // refuses to truncate if any keyspace still lags.
        self.turba
            .rotate_journal()
            .map_err(|e| XyzError::Storage(format!("journal rotate: {e}")))?;

        let elapsed = start.elapsed();
        let msg = format!("Compacted all keyspaces in {:.1}s", elapsed.as_secs_f64());
        tracing::info!("{msg}");

        // Persist total_writes after compaction
        self.persist_total_writes();

        Ok(QueryResult::Ok {
            lid: None,
            message: msg,
        })
    }

    // ── SCRUB ─────────────────────────────────────────────────────────

    /// Verify the on-disk integrity of every keyspace: the data-block checksums
    /// of every live SSTable plus each keyspace's MANIFEST. Read-only — it
    /// surfaces silent bit-rot (alert) but never repairs. Returns a report of
    /// what was verified and any corruption found; each finding is also logged
    /// at error level. Footer/`SSTableMeta` and snapshot coverage are out of
    /// scope for now (tracked with the 3f-meta work).
    pub(super) fn execute_scrub(&self) -> Result<QueryResult> {
        let mut ssts = 0usize;
        let mut blocks = 0usize;
        let mut findings: Vec<String> = Vec::new();

        for (ks, tree) in [
            ("spatial", self.turba.spatial.as_ref()),
            ("identity", self.turba.identity.as_ref()),
            ("dictionary", self.turba.dictionary.as_ref()),
            ("ghosts", self.turba.ghosts.as_ref()),
            ("vectors", self.turba.vectors.as_ref()),
        ] {
            let r = tree.scrub();
            ssts += r.ssts_scanned;
            blocks += r.blocks_scanned;

            if !r.manifest_ok {
                let loc = format!("{ks}: MANIFEST checksum mismatch");
                tracing::error!("SCRUB: {loc}");
                findings.push(loc);
            }
            for sst in &r.corrupt_ssts {
                for &b in &sst.bad_blocks {
                    let loc = format!(
                        "{ks} sst={} (id={}) block={b}",
                        sst.path.display(),
                        sst.table_id
                    );
                    tracing::error!("SCRUB: checksum mismatch at {loc}");
                    findings.push(loc);
                }
            }
        }

        let msg = if findings.is_empty() {
            format!("SCRUB clean: verified {blocks} block(s) across {ssts} SST(s) + 4 MANIFESTs")
        } else {
            format!(
                "SCRUB FOUND {} corruption(s) ({blocks} block(s) across {ssts} SST(s) scanned):\n{}",
                findings.len(),
                findings.join("\n")
            )
        };
        Ok(QueryResult::Ok {
            lid: None,
            message: msg,
        })
    }

    // ── MIGRATE ──────────────────────────────────────────────────────

    /// Migrate the database to the current on-disk format: rehash gravity keys
    /// to the value-only (D1) convention AND rewrite record values to V1.
    ///
    /// D1 rehash: a record's `gravity_hash` lives in its SpatialKey (bytes 2-7),
    /// computed name+value pre-0.8. This recomputes the canonical value-only hash
    /// from the record's fields — exactly as a fresh PUT would place it (the
    /// lobe's `GravitySpec`, else the anchor/LID fallback) — and moves the record
    /// to the new key when it differs. The unique `seq` tail keeps every full key
    /// distinct, so a move can never alias another record's key (no in-place
    /// rehash hazard). Records already at their value-only key don't move.
    ///
    /// After all lobes are processed, every gravity spec is re-persisted at the
    /// value-only format byte and the migration guard is lifted. Re-runnable
    /// (idempotent: a value-only key recomputes to itself → skipped). After
    /// MIGRATE, run COMPACT to reclaim the space left by moved keys.
    pub(super) fn execute_migrate(&self, lobe_name: Option<String>) -> Result<QueryResult> {
        use xyzdb_core::record::{deserialize_record, format_version, serialize_record};

        let start = std::time::Instant::now();

        // Collect lobes to migrate
        let lobe_entries: Vec<(u16, String)> = {
            let lobes = self.lobe_registry.read();
            match lobe_name {
                Some(ref name) => {
                    let config = lobes
                        .get(name)
                        .ok_or_else(|| XyzError::LobeNotFound(name.clone()))?;
                    vec![(config.id, name.clone())]
                }
                None => lobes
                    .list()
                    .iter()
                    .map(|c| (c.id, c.name.clone()))
                    .collect(),
            }
        };

        let mut total_rehashed = 0u64; // gravity key moved name+value → value-only
        let mut total_value_only = 0u64; // value reserialized to V1 in place
        let mut total_skipped = 0u64; // already current (value-only key + V1 value)

        // Per-window commit size (test-overridable via MIGRATE_WINDOW_LIMIT).
        let window_limit = match MIGRATE_WINDOW_LIMIT.load(Ordering::Relaxed) {
            0 => 10_000,
            n => n,
        };
        let mut windows_committed = 0u32; // for the crash-injection knob

        for (lobe_id, lobe_name) in &lobe_entries {
            let prefix = lobe_id.to_be_bytes();
            let spec = self.get_gravity_spec(lobe_name);
            // Pending ops for one flush window (heterogeneous: moves + in-place).
            let mut removes: Vec<Vec<u8>> = Vec::new();
            let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            let mut idents: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

            for entry in self
                .spatial_tree()
                .prefix_iter(&prefix)
                .map_err(|e| XyzError::Storage(e.to_string()))?
            {
                let old_key = &entry.key;
                let old_bytes = &entry.value;
                let version = format_version(old_bytes);

                let fr_guard = self.field_registry.read();
                let fd = fr_guard.get_dict(*lobe_id);
                let record = match deserialize_record(old_bytes, lobe_name, fd) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("MIGRATE: skip corrupt record in {}: {}", lobe_name, e);
                        continue;
                    }
                };
                drop(fr_guard);

                // Value: V1 unless already V1.
                let value: Vec<u8> = if version == 0x01 {
                    old_bytes.to_vec()
                } else {
                    serialize_record(&record)
                };

                // Gravity: the canonical value-only hash for this record.
                let new_hash = spec
                    .as_ref()
                    .and_then(|s| s.compute_hash(&record.fields))
                    .unwrap_or_else(|| {
                        crate::ops::put::compute_record_gravity_hash(
                            self,
                            lobe_name,
                            &record.fields,
                        )
                    });
                // gravity_hash is the u48 BE at key bytes 2..8 (same layout for
                // 22-byte and legacy 10-byte keys).
                let b = old_key.as_slice();
                let old_hash = ((b[2] as u64) << 40)
                    | ((b[3] as u64) << 32)
                    | ((b[4] as u64) << 24)
                    | ((b[5] as u64) << 16)
                    | ((b[6] as u64) << 8)
                    | (b[7] as u64);

                if new_hash == old_hash {
                    // Key unchanged; rewrite the value only if not already V1.
                    if version == 0x01 {
                        total_skipped += 1;
                    } else {
                        puts.push((old_key.to_vec(), value));
                        total_value_only += 1;
                    }
                } else {
                    // Key moves: copy the old key, overwrite the 6 gravity bytes
                    // (2..8) with the value-only hash. The rest of the key — sat
                    // (8..10, reserved 0), z_order_2d (10..16), seq (16..24) — is
                    // preserved from the old key; `seq` is globally unique → no
                    // full-key aliasing.
                    let mut new_key = old_key.to_vec();
                    new_key[2..8].copy_from_slice(&new_hash.to_be_bytes()[2..8]);
                    removes.push(old_key.to_vec());
                    puts.push((new_key.clone(), value));
                    idents.push((record.lid.to_bytes().to_vec(), new_key));
                    total_rehashed += 1;
                }

                if puts.len() >= window_limit {
                    self.commit_migrate_window(&mut removes, &mut puts, &mut idents)?;
                    windows_committed += 1;
                    // Test-only crash injection: abort mid-migration after N
                    // committed windows (the earlier windows are already durable).
                    let abort_after = FORCE_MIGRATE_ABORT_AFTER_WINDOWS.load(Ordering::Relaxed);
                    if abort_after != 0 && windows_committed >= abort_after {
                        return Err(XyzError::Storage(format!(
                            "MIGRATE interrupted after {windows_committed} committed window(s) \
                             (test crash injection)"
                        )));
                    }
                    tracing::info!(
                        "MIGRATE: {} rehashed, {} value-migrated so far...",
                        total_rehashed,
                        total_value_only
                    );
                }
            }
            self.commit_migrate_window(&mut removes, &mut puts, &mut idents)?;
        }

        // Re-persist every gravity spec at the value-only format byte (0x03) so a
        // reload sees the database as migrated, then lift the guard.
        {
            let specs: Vec<(String, GravitySpec)> = self
                .gravity_specs
                .read()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let lobes = self.lobe_registry.read();
            for (lobe, spec) in &specs {
                if let Some(cfg) = lobes.get(lobe) {
                    Self::persist_gravity(&self.turba.dictionary, cfg.id, spec)?;
                }
            }
        }
        self.gravity_needs_migration
            .store(false, std::sync::atomic::Ordering::Relaxed);

        let elapsed = start.elapsed();
        let msg = format!(
            "Migrated: {} gravity keys rehashed (value-only), {} values rewritten to V1, \
             {} already current, in {:.1}s. Run COMPACT to reclaim space.",
            total_rehashed,
            total_value_only,
            total_skipped,
            elapsed.as_secs_f64(),
        );
        tracing::info!("MIGRATE: {msg}");

        Ok(QueryResult::Ok {
            lid: None,
            message: msg,
        })
    }

    /// Apply one MIGRATE flush window atomically: removes first, then puts (so a
    /// put to a key wins over a remove of the same key within the window — moot
    /// here since `seq` makes all full keys distinct, but correct under any
    /// order), then identity redirects. Clears the buffers.
    fn commit_migrate_window(
        &self,
        removes: &mut Vec<Vec<u8>>,
        puts: &mut Vec<(Vec<u8>, Vec<u8>)>,
        idents: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        if removes.is_empty() && puts.is_empty() && idents.is_empty() {
            return Ok(());
        }
        let mut batch = self.turba.batch();
        for k in removes.iter() {
            batch.remove_spatial(k);
        }
        for (k, v) in puts.iter() {
            batch.put_spatial(k, v);
        }
        for (lid, k) in idents.iter() {
            batch.put_identity(lid, k);
        }
        batch
            .commit()
            .map_err(|e| XyzError::Storage(format!("migrate batch: {e}")))?;
        removes.clear();
        puts.clear();
        idents.clear();
        Ok(())
    }
}
