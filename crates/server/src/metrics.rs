//! Prometheus exposition format emitter for `StatsSnapshot`. v0.4 cp
//! 2.2.4. The HTTP-style probe `/metrics` short-circuits in
//! `connection.rs` and calls `serialize_stats_to_prometheus` to produce
//! a `text/plain; version=0.0.4` body that Prometheus scrapers parse
//! directly.
//!
//! **Cardinality cap**: per-ghost-lobe metrics are emitted only for
//! the top-N lobes by `record_count` (default `TOP_N_GHOST_LOBES = 10`).
//! Without the cap, a deployment with hundreds of ghost lobes would emit
//! thousands of time-series, blowing scraper memory + dashboard load.
//! The cap is fixed for v0.4; v0.5 may add an operator-tunable knob.
//!
//! **Format spec**: <https://prometheus.io/docs/instrumenting/exposition_formats/>.
//! Lines have the shape `metric_name{labels} value` with `# HELP` and
//! `# TYPE` headers per metric. All metric names are prefixed `xyzdb_`.

use std::fmt::Write;
use xyzdb_engine::stats::StatsSnapshot;

/// Cardinality cap for per-ghost-lobe time-series. Emit metrics for the
/// top-N lobes by `record_count`; the rest are aggregated into a single
/// `xyzdb_ghost_lobe_records_total{rank="other"}` series.
const TOP_N_GHOST_LOBES: usize = 10;

/// Histogram bucket upper bounds in milliseconds, matching the index
/// layout of `KeyspaceStats::block_cache::pread_service_time_us_histogram`.
/// 10 entries; the 11th bucket of a Prometheus histogram is implicit
/// `+Inf` (= total count).
const BUCKET_BOUNDS_MS: [&str; 10] = ["1", "3", "5", "8", "12", "20", "50", "100", "300", "+Inf"];

/// Convert a `StatsSnapshot` to a Prometheus exposition format string.
pub fn serialize_stats_to_prometheus(snapshot: &StatsSnapshot) -> String {
    let mut out = String::with_capacity(8192);

    // ─── Per-keyspace gauges + counters ──────────────────────────────────
    writeln!(
        &mut out,
        "# HELP xyzdb_keyspace_mem_active_bytes Current active memtable bytes per keyspace."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_keyspace_mem_active_bytes gauge").ok();
    for (name, ks) in &snapshot.keyspaces {
        writeln!(
            &mut out,
            "xyzdb_keyspace_mem_active_bytes{{keyspace=\"{}\"}} {}",
            escape_label(name),
            ks.memory.mem_active_bytes
        )
        .ok();
    }

    writeln!(
        &mut out,
        "\n# HELP xyzdb_keyspace_disk_sst On-disk SSTable file count per keyspace."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_keyspace_disk_sst gauge").ok();
    for (name, ks) in &snapshot.keyspaces {
        writeln!(
            &mut out,
            "xyzdb_keyspace_disk_sst{{keyspace=\"{}\"}} {}",
            escape_label(name),
            ks.disk_sst
        )
        .ok();
    }

    writeln!(
        &mut out,
        "\n# HELP xyzdb_keyspace_version_sum Sum of SSTable counts across all levels."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_keyspace_version_sum gauge").ok();
    for (name, ks) in &snapshot.keyspaces {
        writeln!(
            &mut out,
            "xyzdb_keyspace_version_sum{{keyspace=\"{}\"}} {}",
            escape_label(name),
            ks.version_sum
        )
        .ok();
    }

    writeln!(
        &mut out,
        "\n# HELP xyzdb_keyspace_compact_ok_total Successful background compaction cycles."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_keyspace_compact_ok_total counter").ok();
    for (name, ks) in &snapshot.keyspaces {
        writeln!(
            &mut out,
            "xyzdb_keyspace_compact_ok_total{{keyspace=\"{}\"}} {}",
            escape_label(name),
            ks.compact.compact_ok
        )
        .ok();
    }

    writeln!(
        &mut out,
        "\n# HELP xyzdb_keyspace_compact_err_total Failed background compaction cycles."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_keyspace_compact_err_total counter").ok();
    for (name, ks) in &snapshot.keyspaces {
        writeln!(
            &mut out,
            "xyzdb_keyspace_compact_err_total{{keyspace=\"{}\"}} {}",
            escape_label(name),
            ks.compact.compact_err
        )
        .ok();
    }

    // Per-keyspace block-cache hits/misses counters.
    writeln!(
        &mut out,
        "\n# HELP xyzdb_keyspace_block_cache_hits_total Block cache hits per keyspace."
    )
    .ok();
    writeln!(
        &mut out,
        "# TYPE xyzdb_keyspace_block_cache_hits_total counter"
    )
    .ok();
    for (name, ks) in &snapshot.keyspaces {
        writeln!(
            &mut out,
            "xyzdb_keyspace_block_cache_hits_total{{keyspace=\"{}\"}} {}",
            escape_label(name),
            ks.block_cache.hits
        )
        .ok();
    }

    writeln!(
        &mut out,
        "\n# HELP xyzdb_keyspace_block_cache_misses_total Block cache misses per keyspace."
    )
    .ok();
    writeln!(
        &mut out,
        "# TYPE xyzdb_keyspace_block_cache_misses_total counter"
    )
    .ok();
    for (name, ks) in &snapshot.keyspaces {
        writeln!(
            &mut out,
            "xyzdb_keyspace_block_cache_misses_total{{keyspace=\"{}\"}} {}",
            escape_label(name),
            ks.block_cache.misses
        )
        .ok();
    }

    // Per-keyspace pread service time histogram.
    writeln!(
        &mut out,
        "\n# HELP xyzdb_keyspace_pread_service_time_ms pread() service time, HDD-aligned 10 buckets."
    )
    .ok();
    writeln!(
        &mut out,
        "# TYPE xyzdb_keyspace_pread_service_time_ms histogram"
    )
    .ok();
    for (name, ks) in &snapshot.keyspaces {
        let escaped = escape_label(name);
        let mut cumulative: u64 = 0;
        for (i, bound) in BUCKET_BOUNDS_MS.iter().enumerate() {
            cumulative += ks.block_cache.pread_service_time_us_histogram[i];
            writeln!(
                &mut out,
                "xyzdb_keyspace_pread_service_time_ms_bucket{{keyspace=\"{}\",le=\"{}\"}} {}",
                escaped, bound, cumulative
            )
            .ok();
        }
        // Histogram requires _sum and _count. We don't track sum
        // separately; emit 0 for sum (acceptable per spec — sum is
        // optional in some scrapers). _count is the cumulative bucket
        // value at +Inf.
        writeln!(
            &mut out,
            "xyzdb_keyspace_pread_service_time_ms_count{{keyspace=\"{}\"}} {}",
            escaped, cumulative
        )
        .ok();
        writeln!(
            &mut out,
            "xyzdb_keyspace_pread_service_time_ms_sum{{keyspace=\"{}\"}} 0",
            escaped
        )
        .ok();
    }

    // ─── Global block cache ──────────────────────────────────────────────
    writeln!(
        &mut out,
        "\n# HELP xyzdb_block_cache_capacity_bytes Configured block cache capacity in bytes."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_block_cache_capacity_bytes gauge").ok();
    writeln!(
        &mut out,
        "xyzdb_block_cache_capacity_bytes {}",
        snapshot.block_cache.capacity_bytes
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_block_cache_weight_bytes Current block cache weight in bytes."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_block_cache_weight_bytes gauge").ok();
    writeln!(
        &mut out,
        "xyzdb_block_cache_weight_bytes {}",
        snapshot.block_cache.weight_bytes
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_block_cache_hits_total Total block cache hits across all keyspaces."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_block_cache_hits_total counter").ok();
    writeln!(
        &mut out,
        "xyzdb_block_cache_hits_total {}",
        snapshot.block_cache.hits
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_block_cache_misses_total Total block cache misses across all keyspaces."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_block_cache_misses_total counter").ok();
    writeln!(
        &mut out,
        "xyzdb_block_cache_misses_total {}",
        snapshot.block_cache.misses
    )
    .ok();

    // v0.4 cp 4.2.1: per-lane admission counters. `outcome` is admitted
    // (miss → insert) or skipped (miss → policy declined; only fires
    // for Flush + Compaction when lane_admission_enabled = true).
    writeln!(
        &mut out,
        "\n# HELP xyzdb_block_cache_admission_total Block cache admission decisions per lane (v0.4 cp 4.2.1)."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_block_cache_admission_total counter").ok();
    let lane_names = [
        "user_io_read",
        "writer_durable",
        "flush",
        "compaction",
        "scan",
    ];
    for (i, name) in lane_names.iter().enumerate() {
        writeln!(
            &mut out,
            "xyzdb_block_cache_admission_total{{lane=\"{}\",outcome=\"admitted\"}} {}",
            name, snapshot.block_cache.admission[i].admitted
        )
        .ok();
        writeln!(
            &mut out,
            "xyzdb_block_cache_admission_total{{lane=\"{}\",outcome=\"skipped\"}} {}",
            name, snapshot.block_cache.admission[i].skipped
        )
        .ok();
    }
    writeln!(
        &mut out,
        "\n# HELP xyzdb_block_cache_lane_admission_enabled 1 if the v0.4 lane-aware admission policy is active, 0 if disabled."
    )
    .ok();
    writeln!(
        &mut out,
        "# TYPE xyzdb_block_cache_lane_admission_enabled gauge"
    )
    .ok();
    writeln!(
        &mut out,
        "xyzdb_block_cache_lane_admission_enabled {}",
        if snapshot.block_cache.lane_admission_enabled {
            1
        } else {
            0
        }
    )
    .ok();

    // ─── Ghost subsystem ─────────────────────────────────────────────────
    writeln!(
        &mut out,
        "\n# HELP xyzdb_ghost_count_total Total ghost lobes registered across the engine."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_ghost_count_total gauge").ok();
    writeln!(
        &mut out,
        "xyzdb_ghost_count_total {}",
        snapshot.ghosts.total
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_ghost_auto_candidate_total Auto-promotion candidates evaluated since boot."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_ghost_auto_candidate_total counter").ok();
    writeln!(
        &mut out,
        "xyzdb_ghost_auto_candidate_total {}",
        snapshot.ghosts.auto.candidate_total
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_ghost_auto_candidate_spawn_total Candidates that proceeded to spawn."
    )
    .ok();
    writeln!(
        &mut out,
        "# TYPE xyzdb_ghost_auto_candidate_spawn_total counter"
    )
    .ok();
    writeln!(
        &mut out,
        "xyzdb_ghost_auto_candidate_spawn_total {}",
        snapshot.ghosts.auto.candidate_spawn
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_ghost_auto_dedup_lost_total Candidates that lost the dedup race and threw work away."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_ghost_auto_dedup_lost_total counter").ok();
    writeln!(
        &mut out,
        "xyzdb_ghost_auto_dedup_lost_total {}",
        snapshot.ghosts.auto.dedup_lost
    )
    .ok();

    // Per-ghost-lobe records (top-N + "other" aggregate).
    writeln!(
        &mut out,
        "\n# HELP xyzdb_ghost_lobe_records Records materialised in the top-N ghost lobes by count."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_ghost_lobe_records gauge").ok();
    let mut sorted: Vec<_> = snapshot.ghosts.per_lobe.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.record_count));
    let mut other_total: u64 = 0;
    for (rank, entry) in sorted.iter().enumerate() {
        if rank < TOP_N_GHOST_LOBES {
            writeln!(
                &mut out,
                "xyzdb_ghost_lobe_records{{name=\"{}\",source=\"{}\",rank=\"{}\"}} {}",
                escape_label(&entry.name),
                escape_label(&entry.source_lobe),
                rank,
                entry.record_count
            )
            .ok();
        } else {
            other_total += entry.record_count;
        }
    }
    if other_total > 0 {
        writeln!(
            &mut out,
            "xyzdb_ghost_lobe_records{{rank=\"other\"}} {}",
            other_total
        )
        .ok();
    }

    // ─── Sync thread ─────────────────────────────────────────────────────
    writeln!(
        &mut out,
        "\n# HELP xyzdb_sync_thread_last_successful_ts_ms Unix epoch ms of last successful WAL fsync (group-commit)."
    )
    .ok();
    writeln!(
        &mut out,
        "# TYPE xyzdb_sync_thread_last_successful_ts_ms gauge"
    )
    .ok();
    writeln!(
        &mut out,
        "xyzdb_sync_thread_last_successful_ts_ms {}",
        snapshot.sync_thread.last_successful_sync_ts_ms
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_sync_thread_heartbeat_total Sync thread loop iterations (liveness counter)."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_sync_thread_heartbeat_total counter").ok();
    writeln!(
        &mut out,
        "xyzdb_sync_thread_heartbeat_total {}",
        snapshot.sync_thread.heartbeat_count
    )
    .ok();

    // ─── Process / cgroup memory (Linux) ────────────────────────────────
    writeln!(
        &mut out,
        "\n# HELP xyzdb_process_vmrss_bytes Process VmRSS in bytes (Linux only; 0 elsewhere)."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_process_vmrss_bytes gauge").ok();
    writeln!(
        &mut out,
        "xyzdb_process_vmrss_bytes {}",
        snapshot.process.vmrss_bytes
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_cgroup_anon_bytes cgroup anon memory bytes (Linux only)."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_cgroup_anon_bytes gauge").ok();
    writeln!(
        &mut out,
        "xyzdb_cgroup_anon_bytes {}",
        snapshot.cgroup.anon_bytes
    )
    .ok();

    writeln!(
        &mut out,
        "\n# HELP xyzdb_cgroup_file_bytes cgroup file (page cache) memory bytes (Linux only)."
    )
    .ok();
    writeln!(&mut out, "# TYPE xyzdb_cgroup_file_bytes gauge").ok();
    writeln!(
        &mut out,
        "xyzdb_cgroup_file_bytes {}",
        snapshot.cgroup.file_bytes
    )
    .ok();

    out
}

/// Escape a label value per Prometheus exposition format: backslashes,
/// double quotes, and newlines need backslash-escaping.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            '\n' => out.push_str(r"\n"),
            other => out.push(other),
        }
    }
    out
}
