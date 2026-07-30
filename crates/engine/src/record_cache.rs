use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::lid::LID;
use xyzdb_core::record::Record;

/// Explicit in-memory cache for hot data, controlled by INCACHE/OUTCACHE commands.
/// Budget-enforced with LRU eviction: on budget pressure the oldest-accessed
/// records are evicted before erroring (v0.5.2 — pre-v0.5.2 returned the
/// error immediately without trying to make room).
pub struct RecordCache {
    lobes: DashMap<u16, Arc<DashMap<LID, Record>>>,
    /// Monotonic global counter; incremented on every access (load / get /
    /// scan_lobe hit). Each cached record's last-access value is stored in
    /// `access_clock` so eviction can pick the lowest-clock entries.
    access_counter: AtomicU64,
    /// Per-record last-access timestamp (lobe_id, lid) -> counter snapshot.
    /// Kept in step with `lobes`: an entry exists iff the record is cached.
    access_clock: DashMap<(u16, LID), AtomicU64>,
    budget_bytes: usize,
    used_bytes: AtomicUsize,
}

/// Cache statistics for SHOW CACHE.
pub struct CacheStats {
    pub lobes: Vec<LobeCacheInfo>,
    pub used_bytes: usize,
    pub budget_bytes: usize,
}

pub struct LobeCacheInfo {
    pub lobe_id: u16,
    pub record_count: usize,
    pub estimated_bytes: usize,
}

impl RecordCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            lobes: DashMap::new(),
            access_counter: AtomicU64::new(0),
            access_clock: DashMap::new(),
            budget_bytes,
            used_bytes: AtomicUsize::new(0),
        }
    }

    /// Total bytes currently held by cached records (sum of `estimated_size()`).
    /// Exposed for `/stats` and operator introspection.
    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Relaxed)
    }

    fn touch(&self, lobe_id: u16, lid: LID) {
        let now = self.access_counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.access_clock
            .entry((lobe_id, lid))
            .or_insert_with(|| AtomicU64::new(0))
            .store(now, Ordering::Relaxed);
    }

    /// Evict least-recently-accessed records until at least `target_bytes`
    /// have been freed. Returns the actual number of bytes freed (may be
    /// less than `target_bytes` if the cache holds less than that amount).
    /// Best-effort under concurrent access; eviction order races with
    /// concurrent `get`/`load_records` are acceptable for a soft LRU.
    pub fn evict_lru(&self, target_bytes: usize) -> usize {
        if target_bytes == 0 {
            return 0;
        }
        // Snapshot (lobe_id, lid, clock) for all currently cached records.
        let mut snapshot: Vec<(u16, LID, u64)> = self
            .access_clock
            .iter()
            .map(|e| {
                let (lobe_id, lid) = *e.key();
                let clock = e.value().load(Ordering::Relaxed);
                (lobe_id, lid, clock)
            })
            .collect();
        // Sort ascending by clock so oldest accesses come first.
        snapshot.sort_unstable_by_key(|(_, _, clock)| *clock);

        let mut freed = 0usize;
        for (lobe_id, lid, _) in snapshot {
            if freed >= target_bytes {
                break;
            }
            let Some(map_ref) = self.lobes.get(&lobe_id) else {
                continue;
            };
            let map = map_ref.clone();
            drop(map_ref);
            let Some((_, removed)) = map.remove(&lid) else {
                continue;
            };
            let size = removed.estimated_size();
            self.used_bytes.fetch_sub(
                size.min(self.used_bytes.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            self.access_clock.remove(&(lobe_id, lid));
            freed = freed.saturating_add(size);
        }
        freed
    }

    /// Load records into cache. On budget pressure, evicts least-recently-
    /// accessed records first; only fails if the new batch still does not
    /// fit after evicting every cached record.
    pub fn load_records(&self, lobe_id: u16, records: Vec<Record>) -> Result<usize> {
        let estimated: usize = records.iter().map(|r| r.estimated_size()).sum();

        if estimated > self.budget_bytes {
            return Err(XyzError::InvalidQuery(format!(
                "INCACHE batch ~{}MB exceeds total budget {}MB. Use WHERE to filter.",
                estimated / 1_048_576,
                self.budget_bytes / 1_048_576,
            )));
        }

        let current = self.used_bytes.load(Ordering::Relaxed);
        if current + estimated > self.budget_bytes {
            let needed = (current + estimated).saturating_sub(self.budget_bytes);
            let freed = self.evict_lru(needed);
            // Re-read after eviction; if still over budget the cache cannot
            // make room (concurrent loads racing or estimate skew).
            let after = self.used_bytes.load(Ordering::Relaxed);
            if after + estimated > self.budget_bytes {
                return Err(XyzError::InvalidQuery(format!(
                    "INCACHE would use ~{}MB; budget {}MB ({}MB after evicting {}MB). Use WHERE to filter.",
                    estimated / 1_048_576,
                    self.budget_bytes / 1_048_576,
                    after / 1_048_576,
                    freed / 1_048_576,
                )));
            }
        }

        let map = self
            .lobes
            .entry(lobe_id)
            .or_insert_with(|| Arc::new(DashMap::new()))
            .clone();

        let count = records.len();
        for record in records {
            let lid = record.lid;
            map.insert(lid, record);
            self.touch(lobe_id, lid);
        }

        self.used_bytes.fetch_add(estimated, Ordering::Relaxed);
        Ok(count)
    }

    /// Get a record from cache by LID. Returns None if not cached. A hit
    /// bumps the access clock so the record moves to the back of the LRU
    /// queue.
    pub fn get(&self, lobe_id: u16, lid: &LID) -> Option<Record> {
        let record = self
            .lobes
            .get(&lobe_id)
            .and_then(|map| map.get(lid).map(|r| r.clone()))?;
        self.touch(lobe_id, *lid);
        Some(record)
    }

    /// Check if a lobe has any cached records.
    // parked: record-cache scan-read
    #[allow(dead_code)]
    pub fn has_lobe(&self, lobe_id: u16) -> bool {
        self.lobes.get(&lobe_id).is_some_and(|map| !map.is_empty())
    }

    /// Scan all cached records in a lobe. Returns records matching a filter.
    /// Every record that survives the filter has its access clock bumped
    /// — a SCAN that returns 100 rows touches 100 LRU entries.
    // parked: record-cache scan-read
    #[allow(dead_code)]
    pub fn scan_lobe<F>(&self, lobe_id: u16, filter: F) -> Vec<Record>
    where
        F: Fn(&Record) -> bool,
    {
        match self.lobes.get(&lobe_id) {
            Some(map) => {
                let out: Vec<Record> = map
                    .iter()
                    .filter(|entry| filter(entry.value()))
                    .map(|entry| entry.value().clone())
                    .collect();
                for r in &out {
                    self.touch(lobe_id, r.lid);
                }
                out
            }
            None => vec![],
        }
    }

    /// Write-through: update a record in cache (if the lobe is cached).
    pub fn update_record(&self, lobe_id: u16, record: &Record) {
        if let Some(map) = self.lobes.get(&lobe_id) {
            map.insert(record.lid, record.clone());
            self.touch(lobe_id, record.lid);
        }
    }

    /// Invalidate (remove) a single record from cache.
    pub fn invalidate_record(&self, lobe_id: u16, lid: &LID) {
        if let Some(map) = self.lobes.get(&lobe_id)
            && let Some((_, removed)) = map.remove(lid)
        {
            let size = removed.estimated_size();
            self.used_bytes.fetch_sub(
                size.min(self.used_bytes.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            self.access_clock.remove(&(lobe_id, *lid));
        }
    }

    /// Evict all records for a lobe from cache.
    pub fn evict_lobe(&self, lobe_id: u16) {
        if let Some((_, map)) = self.lobes.remove(&lobe_id) {
            let freed: usize = map.iter().map(|e| e.value().estimated_size()).sum();
            self.used_bytes.fetch_sub(
                freed.min(self.used_bytes.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            // Drop matching access_clock entries.
            self.access_clock.retain(|(l, _), _| *l != lobe_id);
        }
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        let lobes: Vec<LobeCacheInfo> = self
            .lobes
            .iter()
            .map(|entry| {
                let lobe_id = *entry.key();
                let map = entry.value();
                let record_count = map.len();
                let estimated_bytes: usize = map.iter().map(|e| e.value().estimated_size()).sum();
                LobeCacheInfo {
                    lobe_id,
                    record_count,
                    estimated_bytes,
                }
            })
            .collect();

        CacheStats {
            lobes,
            used_bytes: self.used_bytes.load(Ordering::Relaxed),
            budget_bytes: self.budget_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use xyzdb_core::record::Record;
    use xyzdb_core::value::Value;

    fn mk_record(lid_seed: u64, payload_bytes: usize) -> Record {
        let mut fields = BTreeMap::new();
        let padding: String = (0..payload_bytes).map(|_| 'x').collect();
        fields.insert("data".to_string(), Value::Text(padding));
        Record {
            lid: LID::from_raw(lid_seed as u128),
            lobe_name: "test_lobe".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn evict_lru_frees_oldest_when_budget_exceeded() {
        // Size the budget for exactly 2 records; the third forces eviction
        // of the oldest. Sample sizes are computed empirically because
        // estimated_size() includes lobe_name + field overhead beyond
        // the raw payload bytes.
        let r1 = mk_record(1, 4000);
        let r2 = mk_record(2, 4000);
        let r3 = mk_record(3, 4000);
        let one_record = r1.estimated_size();
        // Budget = 2.5 × one record → two fit comfortably, three trigger evict.
        let budget = one_record * 5 / 2;
        let cache = RecordCache::new(budget);
        cache.load_records(1, vec![r1.clone(), r2.clone()]).unwrap();
        // Touch r2 so r1 is the strictly older entry.
        let _ = cache.get(1, &r2.lid);
        // Insert r3; budget would be exceeded → evict the oldest (r1).
        cache.load_records(1, vec![r3.clone()]).unwrap();
        assert!(
            cache.get(1, &r1.lid).is_none(),
            "r1 should have been evicted"
        );
        assert!(cache.get(1, &r2.lid).is_some());
        assert!(cache.get(1, &r3.lid).is_some());
        assert!(
            cache.used_bytes() <= budget,
            "used_bytes must not exceed budget"
        );
    }

    #[test]
    fn evict_lru_explicit_returns_bytes_freed() {
        let cache = RecordCache::new(10 * 1024);
        let r1 = mk_record(1, 1500);
        let r2 = mk_record(2, 1500);
        cache.load_records(1, vec![r1.clone(), r2.clone()]).unwrap();
        let freed = cache.evict_lru(1);
        assert!(freed > 0, "evict_lru(1) should free at least one record");
        // At least r1 evicted; r2 may also be evicted if first record alone
        // does not satisfy the request (which would be unusual at 1500 B).
    }

    #[test]
    fn load_records_fails_when_batch_exceeds_total_budget() {
        let cache = RecordCache::new(1024);
        let r1 = mk_record(1, 2048);
        // Even an empty cache cannot accept this record; LRU does not help.
        let err = cache.load_records(1, vec![r1]).unwrap_err();
        assert!(format!("{err:?}").contains("exceeds total budget"));
    }

    #[test]
    fn evict_lru_target_zero_is_noop() {
        let cache = RecordCache::new(10 * 1024);
        let r1 = mk_record(1, 1500);
        cache.load_records(1, vec![r1.clone()]).unwrap();
        let freed = cache.evict_lru(0);
        assert_eq!(freed, 0);
        assert!(cache.get(1, &r1.lid).is_some(), "noop must not evict");
    }
}
