use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use xytalk_parser::ast::Filter;

/// Default auto-ghost detection thresholds.
///
/// Both are instance fields on `ScanTelemetryRegistry`, not `const`, so
/// `set_thresholds` can override them at runtime — crucial for tests
/// that need to force creation without waiting for the production
/// threshold to fire naturally. Production opens via
/// `ScanTelemetryRegistry::new()` which applies the constants below.
///
/// **`DEFAULT_MIN_LATENCY_MS = 20.0`**: chosen as the threshold where
/// automatic optimization starts paying off. Queries under 20ms rarely
/// justify the notify_write overhead a ghost imposes on every
/// subsequent write; queries over 20ms are where DBAs traditionally
/// start manual tuning. Lower thresholds (e.g. 10ms) cause ghost
/// thrashing under mixed workloads — the LRU cap of 20 Ephemerals per
/// lobe (see `ghost_pool.rs`) turns every transient hot query into a
/// ghost that's evicted before it amortizes its creation cost.
///
/// Empirical re-tuning is expected post-launch if the concurrent
/// benchmark shows specific queries consistently missing the window.
const DEFAULT_MIN_HITS: u64 = 5;
const DEFAULT_MIN_LATENCY_MS: f64 = 20.0;

/// Sliding window over which `DEFAULT_MIN_HITS` must accumulate for a
/// pattern to trip auto-ghost creation. A pattern with 4 hits per hour
/// is not "sustained" — auto-ghost is specifically for bursty hot
/// patterns. Periodic workloads (weekly/monthly reports) intentionally
/// fall outside this window and must be handled by manual `CREATE GHOST`.
const PATTERN_WINDOW: Duration = Duration::from_secs(600);

/// Per-pattern cap on buffered hit timestamps. `DEFAULT_MIN_HITS` is 5
/// and the window is 10 minutes; anything beyond 100 recent hits is
/// noise for the decision. The cap bounds memory per pattern at a
/// predictable constant.
const RECENT_HITS_CAP: usize = 100;

/// Record of a single SCAN execution.
#[derive(Debug, Clone)]
pub struct ScanTelemetry {
    pub lobe: String,
    pub filter_desc: String,
    pub source: String,
    pub records_scanned: u64,
    pub records_returned: u64,
    pub duration: Duration,
}

/// A scan pattern that might benefit from a Ghost Lobe.
struct ScanPattern {
    lobe: String,
    filter_desc: String,
    filters: Vec<Filter>,
    /// V3: Fields used in AGGREGATE pipelines for this pattern.
    aggregate_fields: Vec<String>,
    hit_count: u64,
    avg_latency_ms: f64,
    /// Has an auto-ghost already been created for this pattern?
    ghost_created: bool,
    /// Hit timestamps, bounded at `RECENT_HITS_CAP`. Filtered by
    /// `elapsed() < PATTERN_WINDOW` at trigger-check time, so stale
    /// entries don't count toward the threshold even while they occupy
    /// buffer slots. Purge-on-read is cheaper than purge-on-write for
    /// patterns that go hot-then-cold — the eventual drop-off is
    /// handled by the cap, the window check handles correctness.
    recent_hits: VecDeque<Instant>,
}

/// Candidate for auto-ghost creation returned by the telemetry store.
pub struct AutoGhostCandidate {
    pub lobe: String,
    pub filters: Vec<Filter>,
    pub filter_desc: String,
    /// V3: Fields from AGGREGATE pipelines to include in ghost projection.
    pub aggregate_fields: Vec<String>,
}

/// Stores recent scan telemetry and detects painful patterns.
pub struct ScanTelemetryRegistry {
    recent: VecDeque<ScanTelemetry>,
    /// Patterns: filter_desc → ScanPattern
    patterns: HashMap<String, ScanPattern>,
    /// Auto-ghost trigger: a pattern must hit this many times within
    /// `PATTERN_WINDOW`. Field (not const) so tests can lower it.
    min_hits: u64,
    /// Auto-ghost trigger: the pattern's rolling average latency must
    /// exceed this. Field (not const) so tests can zero it out.
    min_latency_ms: f64,
}

impl Default for ScanTelemetryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanTelemetryRegistry {
    pub fn new() -> Self {
        Self {
            recent: VecDeque::with_capacity(1024),
            patterns: HashMap::new(),
            min_hits: DEFAULT_MIN_HITS,
            min_latency_ms: DEFAULT_MIN_LATENCY_MS,
        }
    }

    /// Record a scan execution. Returns an auto-ghost candidate if the
    /// pattern has accumulated `min_hits` within `PATTERN_WINDOW` AND
    /// its rolling average latency exceeds `min_latency_ms`.
    ///
    /// `aggregate_fields` — fields used in AGGREGATE pipeline (ghost
    /// projection hint).
    pub fn record_with_filters(
        &mut self,
        t: ScanTelemetry,
        filters: &[Filter],
        aggregate_fields: &[String],
    ) -> Option<AutoGhostCandidate> {
        let ms = t.duration.as_secs_f64() * 1000.0;
        let lobe = t.lobe.clone();
        let filter_desc = t.filter_desc.clone();
        let min_hits = self.min_hits;
        let min_latency_ms = self.min_latency_ms;

        let pattern = self
            .patterns
            .entry(filter_desc.clone())
            .or_insert_with(|| ScanPattern {
                lobe: lobe.clone(),
                filter_desc: filter_desc.clone(),
                filters: filters.to_vec(),
                aggregate_fields: Vec::new(),
                hit_count: 0,
                avg_latency_ms: 0.0,
                ghost_created: false,
                recent_hits: VecDeque::with_capacity(RECENT_HITS_CAP),
            });

        // Merge new aggregate fields into pattern (accumulate across repeated scans)
        for field in aggregate_fields {
            if !pattern.aggregate_fields.contains(field) {
                pattern.aggregate_fields.push(field.clone());
            }
        }

        pattern.hit_count += 1;
        pattern.avg_latency_ms = (pattern.avg_latency_ms * (pattern.hit_count - 1) as f64 + ms)
            / pattern.hit_count as f64;

        // Record this hit's timestamp with a hard cap. The eventual window
        // filter below handles accuracy — the cap just bounds memory.
        pattern.recent_hits.push_back(Instant::now());
        while pattern.recent_hits.len() > RECENT_HITS_CAP {
            pattern.recent_hits.pop_front();
        }

        self.recent.push_back(t);
        if self.recent.len() > 1000 {
            self.recent.pop_front();
        }

        // Count hits within the sliding window. Stale entries in
        // `recent_hits` are buffered but filtered out here.
        let hits_in_window = pattern
            .recent_hits
            .iter()
            .filter(|ts| ts.elapsed() < PATTERN_WINDOW)
            .count() as u64;

        if !pattern.ghost_created
            && !filters.is_empty()
            && hits_in_window >= min_hits
            && pattern.avg_latency_ms >= min_latency_ms
        {
            pattern.ghost_created = true;
            Some(AutoGhostCandidate {
                lobe: pattern.lobe.clone(),
                filters: pattern.filters.clone(),
                filter_desc: pattern.filter_desc.clone(),
                aggregate_fields: pattern.aggregate_fields.clone(),
            })
        } else {
            None
        }
    }

    /// Override the auto-ghost trigger thresholds. Intended for tests
    /// that need to force creation in controlled conditions (e.g. "5
    /// scans in 2 seconds must trigger") without waiting for the
    /// production threshold to fire naturally. Production never calls
    /// this — `new()` applies the `DEFAULT_*` constants. Gated behind
    /// `#[cfg(test)]` so release builds do not carry the symbol; all
    /// callers live in `#[cfg(test)] mod tests` in `engine.rs`.
    #[cfg(test)]
    pub(crate) fn set_thresholds(&mut self, min_hits: u64, min_latency_ms: f64) {
        self.min_hits = min_hits;
        self.min_latency_ms = min_latency_ms;
    }

    /// Set `min_hits` in isolation. Operator-facing: called via the public
    /// `Engine::set_auto_ghost_thresholds` so a CLI flag can tune one
    /// threshold without stomping the other.
    pub(crate) fn set_min_hits(&mut self, min_hits: u64) {
        self.min_hits = min_hits;
    }

    /// Set `min_latency_ms` in isolation. See `set_min_hits`.
    pub(crate) fn set_min_latency_ms(&mut self, min_latency_ms: f64) {
        self.min_latency_ms = min_latency_ms;
    }

    /// Number of patterns currently tracked. Diagnostic getter used by
    /// the e2e test's failure-path error message.
    pub(crate) fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    /// Number of recent scan records buffered (bounded at 1000). Gated
    /// behind `#[cfg(test)]`: the sole caller is in `#[cfg(test)] mod
    /// tests` in `engine.rs` (diagnostic error message on a failing
    /// auto-ghost assertion).
    #[cfg(test)]
    pub(crate) fn recent_count(&self) -> usize {
        self.recent.len()
    }

    /// Record a ghost-routed scan for diagnostics only. Appends to
    /// `recent` (so `SHOW SCAN STATS` reflects the routed scan) but does
    /// NOT touch `patterns` — a scan that hit a ghost in 0.5ms would
    /// otherwise deflate the pattern's average latency and suppress
    /// future auto-ghost candidates for neighboring uncovered queries.
    ///
    /// Symmetric with `record_with_filters`, which is now only called
    /// for `ScanSource::Primary` (the routing decision lives in
    /// `ops/scan.rs`).
    pub fn record_routed(&mut self, t: ScanTelemetry) {
        self.recent.push_back(t);
        if self.recent.len() > 1000 {
            self.recent.pop_front();
        }
    }

    /// Simple record (without auto-ghost check).
    ///
    // TODO(streaming-scan-config): Streaming scans (`execute_scan_streaming`)
    // call this path, which updates `patterns` and contributes to
    // avg_latency_ms but never generates an AutoGhostCandidate (no threshold
    // check). That's a pre-v0.2 quirk: streaming hot patterns are "seen" for
    // diagnostic purposes but never trigger auto-ghost creation.
    // Normalize this when thresholds become configurable — either
    // emit candidates from streaming too, or stop touching patterns here.
    pub fn record(&mut self, t: ScanTelemetry) {
        let ms = t.duration.as_secs_f64() * 1000.0;
        let filter_desc = t.filter_desc.clone();
        let lobe = t.lobe.clone();

        let pattern = self
            .patterns
            .entry(filter_desc.clone())
            .or_insert_with(|| ScanPattern {
                lobe,
                filter_desc,
                filters: Vec::new(),
                aggregate_fields: Vec::new(),
                hit_count: 0,
                avg_latency_ms: 0.0,
                ghost_created: false,
                recent_hits: VecDeque::with_capacity(RECENT_HITS_CAP),
            });
        pattern.hit_count += 1;
        pattern.avg_latency_ms = (pattern.avg_latency_ms * (pattern.hit_count - 1) as f64 + ms)
            / pattern.hit_count as f64;

        self.recent.push_back(t);
        if self.recent.len() > 1000 {
            self.recent.pop_front();
        }
    }

    /// Set the `ghost_created` flag for a pattern. Symmetric API used by
    /// both directions: set to `true` to suppress re-triggering when a
    /// ghost exists, set to `false` when the ghost disappears (TTL expiry,
    /// LRU eviction) so the filter can re-trigger auto-ghost creation if
    /// the pattern stays hot.
    ///
    /// Silent no-op if `filter_desc` has no recorded pattern. Previously
    /// this was the split pair `mark_ghost_exists` / (nothing for clear);
    /// consolidated when the clear direction was needed.
    pub fn set_ghost_flag(&mut self, filter_desc: &str, exists: bool) {
        if let Some(p) = self.patterns.get_mut(filter_desc) {
            p.ghost_created = exists;
        }
    }

    /// Read the `ghost_created` flag for a pattern. Returns `None` if no
    /// pattern matches — distinguishes "not recorded" from "recorded but
    /// no ghost yet." Mainly for integration tests that verify the
    /// clear-on-drop semantics from the reaper.
    #[cfg(test)]
    pub fn has_ghost_flag(&self, filter_desc: &str) -> Option<bool> {
        self.patterns.get(filter_desc).map(|p| p.ghost_created)
    }

    /// Generate SHOW SCAN STATS output.
    pub fn format_stats(&self) -> Vec<String> {
        let mut lines = vec!["Scan Statistics:".to_string()];

        if self.recent.is_empty() {
            lines.push("  No scans recorded yet.".to_string());
            return lines;
        }

        lines.push(format!("  Total scans recorded: {}", self.recent.len()));
        lines.push(String::new());

        // Show painful patterns
        let mut painful: Vec<&ScanPattern> = self
            .patterns
            .values()
            .filter(|p| p.hit_count >= 3 && p.avg_latency_ms > 100.0)
            .collect();
        painful.sort_by(|a, b| {
            b.avg_latency_ms
                .partial_cmp(&a.avg_latency_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if painful.is_empty() {
            lines.push("  No slow scan patterns detected.".to_string());
        } else {
            lines.push("  Slow scan patterns (avg > 100ms):".to_string());
            for p in painful.iter().take(10) {
                let ghost_tag = if p.ghost_created { " [auto-ghost]" } else { "" };
                lines.push(format!(
                    "    {} — {} times, avg {:.1}ms{}",
                    p.filter_desc, p.hit_count, p.avg_latency_ms, ghost_tag
                ));
            }
        }

        // Last 5 scans
        lines.push(String::new());
        lines.push("  Recent scans:".to_string());
        for t in self.recent.iter().rev().take(5) {
            lines.push(format!(
                "    {} | {} | {}/{} records | {:.1}ms",
                t.lobe,
                t.source,
                t.records_returned,
                t.records_scanned,
                t.duration.as_secs_f64() * 1000.0,
            ));
        }

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry(filter_desc: &str, source: &str, latency_ms: f64) -> ScanTelemetry {
        ScanTelemetry {
            lobe: "data".into(),
            filter_desc: filter_desc.into(),
            source: source.into(),
            records_scanned: 0,
            records_returned: 0,
            duration: Duration::from_secs_f64(latency_ms / 1000.0),
        }
    }

    /// Ghost-routed scans must NOT contribute to the pattern store. A ghost
    /// read at 0.5ms would deflate avg_latency_ms for the pattern below the
    /// AUTO_GHOST_MIN_LATENCY_MS threshold and starve detection of similar
    /// uncovered patterns.
    #[test]
    fn record_routed_leaves_patterns_untouched() {
        let mut store = ScanTelemetryRegistry::new();

        // First, prove the pattern store DOES respond to record_with_filters.
        let filters: Vec<xytalk_parser::ast::Filter> = vec![];
        let _ =
            store.record_with_filters(telemetry("status=active", "primary", 100.0), &filters, &[]);
        assert_eq!(store.patterns.len(), 1, "primary scan seeds the pattern");

        // Now fire many ghost-routed scans for the SAME filter_desc.
        // `record_routed` must leave the pattern state alone.
        let hits_before = store.patterns["status=active"].hit_count;
        let avg_before = store.patterns["status=active"].avg_latency_ms;
        for _ in 0..10 {
            store.record_routed(telemetry("status=active", "ghost:auto_x", 0.5));
        }
        let hits_after = store.patterns["status=active"].hit_count;
        let avg_after = store.patterns["status=active"].avg_latency_ms;

        assert_eq!(
            hits_before, hits_after,
            "record_routed must not bump hit_count"
        );
        assert!(
            (avg_before - avg_after).abs() < f64::EPSILON,
            "record_routed must not perturb avg_latency_ms"
        );

        // But the routed scans DO appear in `recent` for SHOW SCAN STATS.
        assert_eq!(store.recent.len(), 11, "1 primary + 10 routed in recent");
    }

    #[test]
    fn set_ghost_flag_round_trips() {
        let mut store = ScanTelemetryRegistry::new();
        let filters: Vec<xytalk_parser::ast::Filter> = vec![];

        // Seed a pattern by recording one primary scan.
        store.record_with_filters(telemetry("f=x", "primary", 1.0), &filters, &[]);
        assert_eq!(store.has_ghost_flag("f=x"), Some(false));

        store.set_ghost_flag("f=x", true);
        assert_eq!(store.has_ghost_flag("f=x"), Some(true));

        store.set_ghost_flag("f=x", false);
        assert_eq!(store.has_ghost_flag("f=x"), Some(false));
    }

    /// Clearing a flag for a pattern that was never recorded must be a
    /// silent no-op — callers in the reaper / LRU-evict paths invoke
    /// this on every dropped ghost, and not every dropped ghost came
    /// from telemetry (e.g. manually-created CREATE GHOST ghosts have
    /// no pattern entry).
    #[test]
    fn set_ghost_flag_missing_pattern_is_noop() {
        let mut store = ScanTelemetryRegistry::new();
        store.set_ghost_flag("never_recorded", false);
        assert_eq!(store.has_ghost_flag("never_recorded"), None);
    }

    #[test]
    fn record_routed_bounds_recent_to_1000_entries() {
        let mut store = ScanTelemetryRegistry::new();
        for i in 0..1050 {
            store.record_routed(telemetry(&format!("f{i}"), "ghost:g", 1.0));
        }
        assert_eq!(store.recent.len(), 1000, "recent is capped at 1000");
    }
}
