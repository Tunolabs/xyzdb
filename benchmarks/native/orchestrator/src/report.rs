//! Per-run report writers: JSON, CSV, Markdown.

use anyhow::{Context, Result};
use native_generator::bench::RunReport;
use std::path::Path;

pub fn write_json(out_dir: &Path, run_id: &str, report: &RunReport) -> Result<()> {
    let path = out_dir.join(format!("{run_id}.json"));
    let text = serde_json::to_string_pretty(report).context("serialise run report to JSON")?;
    std::fs::write(&path, text).with_context(|| format!("write {:?}", path))?;
    Ok(())
}

pub fn write_csv(out_dir: &Path, run_id: &str, report: &RunReport) -> Result<()> {
    let path = out_dir.join(format!("{run_id}.csv"));
    let mut wtr = csv::Writer::from_path(&path).with_context(|| format!("open csv {:?}", path))?;
    wtr.write_record([
        "query",
        "n_runs",
        "p50_ms",
        "p95_ms",
        "p99_ms",
        "max_ms",
        "avg_ms",
        "avg_records",
        "empty_result_set",
    ])?;
    for q in &report.cold_queries {
        wtr.write_record([
            &q.query,
            &q.n_runs.to_string(),
            &format!("{:.4}", q.p50_ms),
            &format!("{:.4}", q.p95_ms),
            &format!("{:.4}", q.p99_ms),
            &format!("{:.4}", q.max_ms),
            &format!("{:.4}", q.avg_ms),
            &format!("{:.2}", q.avg_records),
            &q.empty_result_set.to_string(),
        ])?;
    }
    wtr.flush()?;
    Ok(())
}

pub fn write_markdown(out_dir: &Path, run_id: &str, report: &RunReport) -> Result<()> {
    let path = out_dir.join(format!("{run_id}.md"));
    let mut md = String::new();
    md.push_str(&format!("# native bench — `{}`\n\n", run_id));
    md.push_str(&format!("- **Engine**: `{}`\n", report.engine.as_str()));
    if !report.engine_image.is_empty() {
        md.push_str(&format!(
            "- **Image / arch**: `{}` (bit-identical recall across arches — v2==v3 gate)\n",
            report.engine_image
        ));
    }
    md.push_str(&format!("- **Storage**: `{}`\n", report.storage.as_str()));
    md.push_str(&format!("- **Scale**: {}\n", report.scale));
    md.push_str(&format!("- **Schema mode**: `{:?}`\n", report.schema_mode));
    md.push_str(&format!(
        "- **Started**: {}\n",
        report.started_at.to_rfc3339()
    ));
    md.push_str(&format!(
        "- **Finished**: {}\n",
        report.finished_at.to_rfc3339()
    ));
    md.push_str(&format!(
        "- **Wall clock**: {} s\n\n",
        (report.finished_at - report.started_at).num_seconds()
    ));

    md.push_str("## Phase 0 — schema\n\n");
    md.push_str(&format!(
        "- Setup statements: {}\n- Setup duration: {} ms\n\n",
        report.schema.setup_statements, report.schema.setup_duration_ms
    ));

    md.push_str("## Phase 1 — bulk load\n\n");
    md.push_str(&format!(
        "- Records loaded: {}\n- Duration: {} ms\n- Rate: {:.0} rec/s\n\n",
        report.load.records_loaded, report.load.duration_ms, report.load.records_per_sec
    ));

    md.push_str("## Phase 2 — cold queries\n\n");
    md.push_str(
        "| Query | n | P50 (ms) | P95 (ms) | P99 (ms) | max (ms) | avg (ms) | avg records | empty? |\n",
    );
    md.push_str("|---|---:|---:|---:|---:|---:|---:|---:|:--:|\n");
    for q in &report.cold_queries {
        let empty_marker = if q.empty_result_set {
            "⚠ EMPTY"
        } else {
            "—"
        };
        md.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1} | {} |\n",
            q.query,
            q.n_runs,
            q.p50_ms,
            q.p95_ms,
            q.p99_ms,
            q.max_ms,
            q.avg_ms,
            q.avg_records,
            empty_marker
        ));
    }
    let empty_qs: Vec<&str> = report
        .cold_queries
        .iter()
        .filter(|q| q.empty_result_set)
        .map(|q| q.query.as_str())
        .collect();
    if !empty_qs.is_empty() {
        md.push_str(&format!(
            "\n> **WARN refinement #16 empty_result_set**: {} executed without errors but returned zero records across every cold repetition. Integrity-pending — investigate before publishing comparative numbers (see the cross-engine bench design notes §12.3).\n",
            empty_qs.join(", ")
        ));
    }
    md.push_str(
        "\n> **avg records** is a per-query, per-engine result-size check that the \
         three engines return the same magnitude. It is NOT comparable across \
         queries: Q7 is a 100-row write batch; Q2/Q5/Q8 are aggregate group \
         counts; Q3/Q6 are capped row counts. Read it across engines for one \
         row, never down the column.\n\n",
    );

    if let Some(c) = &report.concurrent {
        md.push_str("## Phase 3 — concurrent workload\n\n");
        md.push_str(&format!(
            "- Reads total: {} ({:.1}/s)\n- Writes total: {} ({:.1}/s)\n",
            c.reads_total, c.reads_per_sec, c.writes_total, c.writes_per_sec
        ));
        if c.refresh_count > 0 {
            md.push_str(&format!(
                "- Refresh count: {}  (total wall {} ms, avg {:.1} ms/refresh)\n",
                c.refresh_count,
                c.refresh_total_ms,
                c.refresh_total_ms as f64 / c.refresh_count.max(1) as f64
            ));
        }
        md.push_str("\nPer-query stats:\n\n");
        md.push_str("| Query | n | P50 (ms) | P95 (ms) | P99 (ms) | avg (ms) |\n");
        md.push_str("|---|---:|---:|---:|---:|---:|\n");
        for q in &c.per_query {
            md.push_str(&format!(
                "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
                q.query, q.n_runs, q.p50_ms, q.p95_ms, q.p99_ms, q.avg_ms
            ));
        }
        md.push('\n');
    }

    md.push_str("## Phase 5 — integrity verify\n\n");
    md.push_str(&format!(
        "- Exact: **{}**\n",
        if report.verify.exact { "YES" } else { "NO" }
    ));
    if !report.verify.diffs.is_empty() {
        md.push_str("- Diffs:\n\n");
        md.push_str("| Entity | Expected | Observed | Δ |\n|---|---:|---:|---:|\n");
        for d in &report.verify.diffs {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                d.entity,
                d.expected,
                d.observed,
                (d.observed as i64) - (d.expected as i64)
            ));
        }
    }
    if !report.verify.exact {
        // Q7 batch-inserts synthetic payments in Phases 2-3, before this check.
        // The count is per-lobe/-table, so the payment-bearing entity (xyzDB
        // `creditos`, PG `payments`, Mongo `payments`) reads higher than the
        // golden expectation by the number of Q7 inserts. This positive Δ is
        // expected write load, symmetric across the three engines — not a
        // data-integrity failure. Any OTHER entity diff is a real discrepancy.
        md.push_str(
            "\n> **Note**: a positive Δ on the payment-bearing entity is Q7's \
             synthetic batch-insert load (Phases 2-3), expected and symmetric \
             across engines — not a data-integrity failure. Any other entity \
             should match exactly.\n",
        );
    }

    // Phase 1.5 — verify_golden (Phase E Session 2 + caveat C-9 reschedule).
    // Section is omitted entirely when no golden file was loaded so the
    // absence is visible in downstream gates (the integrity-pending
    // state per design doc §12.3 Verify-golden methodology).
    if let Some(g) = &report.verify_golden {
        md.push_str("\n## Phase 1.5 — verify_golden\n\n");
        md.push_str(&format!(
            "- Overall match: **{}**\n",
            if g.overall_match { "YES" } else { "NO" }
        ));
        md.push_str(&format!("- Diffs: {}\n", g.diffs.len()));
        if !g.diffs.is_empty() {
            md.push_str(
                "\n| V-query | Field | Expected | Observed | Rel. Δ |\n|---|---|---:|---:|---:|\n",
            );
            for d in &g.diffs {
                md.push_str(&format!(
                    "| {} | {} | {:.2} | {:.2} | {:.6} |\n",
                    d.query, d.field, d.expected, d.observed, d.relative_delta
                ));
            }
            // With the three current engines (xyzDB, PG, Mongo) any golden
            // diff is a real discrepancy — there is no expected-diff caveat to
            // annotate away (the former Surreal C-2 bulk_load caveat was
            // retired with the SurrealDB driver).
            md.push_str(
                "\n> See the cross-engine bench design notes §12.3 for the \
                 verify-golden methodology; every diff above is a real \
                 discrepancy to investigate.\n",
            );
        }
    }

    // Phase 1.6 — content gate (per-lobe content-hash read-back). Printed so a
    // green integrity line is never read as "everything was verified": the
    // `scope` string states exactly what the gate hashes and what it does not.
    if let Some(cg) = &report.content_gate {
        md.push_str("\n## Phase 1.6 — content gate\n\n");
        if !cg.ran {
            md.push_str(&format!("- Ran: **NO** — {}\n", cg.scope));
        } else {
            md.push_str(&format!(
                "- Ran: **YES** — overall match: **{}**\n- Scope: {}\n",
                if cg.overall_match { "YES" } else { "NO" },
                cg.scope
            ));
            if !cg.lobes.is_empty() {
                md.push_str(
                    "\n| Lobe | Matched | Records hashed | Expected | Observed |\n|---|:--:|---:|---|---|\n",
                );
                for l in &cg.lobes {
                    let trunc = |h: &str| h.chars().take(16).collect::<String>();
                    md.push_str(&format!(
                        "| {} | {} | {} | `{}` | `{}` |\n",
                        l.lobe,
                        if l.matched { "✓" } else { "✗" },
                        l.records_hashed,
                        trunc(&l.expected_hash),
                        trunc(&l.observed_hash),
                    ));
                }
            }
        }
    }

    if let Some(r) = &report.resources {
        md.push_str("\n## Resources\n\n");
        md.push_str(&format!(
            "- Container: `{}`\n- Data path: `{}`\n- Samples: {}\n\n",
            r.container, r.data_path, r.n_samples
        ));
        md.push_str("| Metric | Peak | Avg | Final |\n|---|---:|---:|---:|\n");
        md.push_str(&format!(
            "| CPU % | {:.1} | {:.1} | — |\n",
            r.cpu_peak, r.cpu_avg
        ));
        md.push_str(&format!(
            "| Memory (MiB) | {:.1} | {:.1} | — |\n",
            r.mem_peak_mb, r.mem_avg_mb
        ));
        md.push_str(&format!(
            "| Disk (MiB) | {:.1} | — | {:.1} |\n",
            r.disk_peak_mb, r.disk_final_mb
        ));
        if !r.samples.is_empty() {
            md.push_str("\nPer-phase peak (CPU% / mem MiB):\n\n");
            md.push_str("| Phase | n | CPU % peak | Mem MiB peak | Disk MiB peak |\n|---|---:|---:|---:|---:|\n");
            let mut by_phase: std::collections::BTreeMap<String, (f64, f64, f64, usize)> =
                Default::default();
            for s in &r.samples {
                let e = by_phase.entry(s.phase.clone()).or_default();
                e.0 = e.0.max(s.cpu_percent);
                e.1 = e.1.max(s.mem_mb);
                e.2 = e.2.max(s.disk_mb);
                e.3 += 1;
            }
            for (phase, (cpu, mem, disk, n)) in by_phase {
                md.push_str(&format!(
                    "| {} | {} | {:.1} | {:.1} | {:.1} |\n",
                    phase, n, cpu, mem, disk
                ));
            }
        }
    }

    std::fs::write(&path, md).with_context(|| format!("write {:?}", path))?;
    Ok(())
}
