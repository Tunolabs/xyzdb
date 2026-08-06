//! DIAGNOSTIC (not a gate): isolate the per-query memory balloon of NEAREST in
//! the **memtable** regime — the regime where `density-findings.md` measured
//! anon jump 48→211 MiB/agent under 10 queries and decay back to ~50 in ~75 s.
//!
//! The question this answers: **what grows per query, and does it come back?**
//! - If the per-query spike is ~bucket-size (~32 MiB) → the scan materialises the
//!   bucket → a contiguous f32 vector column (reads 1 KB/record, not the 4 KB blob)
//!   fixes BOTH latency and balloon → worth the column build.
//! - If the spike is small and `live` returns to baseline → it is allocator scratch
//!   that frees but the OS does not reclaim → a cheaper fix (per-query arena /
//!   return memory to the OS) attacks the balloon without touching SELECT/FOLLOW.
//! - If `live` grows monotonically across batches → genuine retention (a leak) →
//!   find what is held.
//!
//! Instrument: a counting global allocator (this is a separate test binary, so it
//! can install its own `#[global_allocator]` — the only one in the workspace is in
//! `xyzdb-server`'s binary, no collision). It tracks live bytes and a resettable
//! high-water, so we read engine allocations directly without `/proc` or cgroups
//! (neither exists on the macOS host). This measures ALLOCATIONS, not OS page
//! residency; for "does the OS give it back" the signal is `live` returning to
//! baseline (allocator freed) — any anon the cgroup still showed is then held by
//! the allocator, not by the engine.
//!
//! Run:  cargo test --release -p xyzdb-engine anon_probe -- --ignored --nocapture

// SPDX-License-Identifier: BUSL-1.1
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};

use xyzdb_engine::engine::Engine;

// ─── counting allocator ─────────────────────────────────────────────────────
static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

struct Counting;

/// Bump `PEAK` up to `v` if `v` is higher (single max-tracking CAS loop).
fn bump_peak(v: i64) {
    let mut p = PEAK.load(Ordering::Relaxed);
    while v > p {
        match PEAK.compare_exchange_weak(p, v, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(cur) => p = cur,
        }
    }
}

// SAFETY: delegates every allocation to the system allocator; the atomics never
// allocate, so there is no re-entrancy.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded verbatim to the system allocator (see impl-level note).
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let live = LIVE.fetch_add(l.size() as i64, Ordering::Relaxed) + l.size() as i64;
            bump_peak(live);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: forwarded verbatim to the system allocator (see impl-level note).
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size() as i64, Ordering::Relaxed);
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded verbatim to the system allocator (see impl-level note).
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            let live = LIVE.fetch_add(l.size() as i64, Ordering::Relaxed) + l.size() as i64;
            bump_peak(live);
        }
        p
    }
    // realloc falls back to the default alloc+copy+dealloc, so it is accounted via
    // the three primitives above.
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> i64 {
    LIVE.load(Ordering::Relaxed)
}
fn reset_peak_to_live() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}
fn peak() -> i64 {
    PEAK.load(Ordering::Relaxed)
}
fn mib(bytes: i64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

const DIM: usize = 256; // embedding width
const N: usize = 8000; // one gravity bucket ~= one agent (~33 MiB of data)
const TEXT_BYTES: usize = 3000; // ~3 KB text/record → ~4 KB blob (vec 1 KB + text 3 KB)

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:?}")).collect();
    format!("[{}]", parts.join(","))
}

/// A deterministic, non-degenerate vector for record/query `i` (no RNG dep).
fn gen_vec(i: usize) -> Vec<f32> {
    (0..DIM)
        .map(|d| {
            let x = ((i
                .wrapping_mul(2654435761)
                .wrapping_add(d.wrapping_mul(40503)))
                % 1000) as f32;
            x / 1000.0 - 0.5
        })
        .collect()
}

#[test]
#[ignore]
fn anon_probe_memtable() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "mem""#).unwrap();
    engine.run(r#"VECTOR emb IN "mem""#).unwrap();

    // Seed one bucket, kept in the memtable (no SST flush forced) — the density
    // regime. The text field makes the blob ~4 KB so the vec is ~1/4 of it.
    let text = "w".repeat(TEXT_BYTES);
    for i in 0..N {
        let q = format!(
            r#"PUT {{*conv:"c1", id:"r{i}", note:"{text}", emb:{}}} IN "mem""#,
            vec_literal(&gen_vec(i))
        );
        engine.run(&q).unwrap();
    }

    // Build the query string ONCE, outside the measured region, so the per-query
    // delta is engine work only (not the test's format!/vec_literal scratch).
    let query = format!(
        r#"SCAN "mem" WHERE conv="c1" LIMIT 10000 | NEAREST(emb, {}, 10, cosine)"#,
        vec_literal(&gen_vec(123_456))
    );

    // A few warm queries: prime any one-time lazy allocations so they do not count
    // as "per-query growth".
    for _ in 0..3 {
        let _ = engine.run(&query).unwrap();
    }

    let baseline = live();
    println!("\n=== anon_probe (memtable) — N={N} dim={DIM} text≈{TEXT_BYTES}B ===");
    println!(
        "data ≈ {:.1} MiB | LIVE baseline (post-seed, post-warm) = {:.1} MiB",
        mib(N as i64 * 4096),
        mib(baseline)
    );

    // ── per-query transient: reset high-water to current live, run ONE query,
    //    read the high-water, drop the result, read live again. ──────────────
    println!("\n-- per-query transient (spike above baseline) & retained-after-drop --");
    let mut spikes = Vec::new();
    for n in 0..8 {
        reset_peak_to_live();
        let pre = live();
        let r = engine.run(&query).unwrap();
        let spike = peak() - pre; // working-set spike DURING the query (result still alive)
        drop(r);
        let retained = live() - pre; // what did NOT free after the result dropped
        spikes.push(spike);
        if n < 4 {
            println!(
                "  q{n}: spike +{:.3} MiB | retained-after-drop {:+.3} MiB",
                mib(spike),
                mib(retained)
            );
        }
    }
    spikes.sort();
    println!(
        "  median single-query spike: +{:.3} MiB",
        mib(spikes[spikes.len() / 2])
    );

    // ── batch of 30 queries: peak above baseline + retained after the batch ──
    println!("\n-- batch of 30 queries --");
    reset_peak_to_live();
    let before_batch = live();
    for _ in 0..30 {
        let r = engine.run(&query).unwrap();
        drop(r);
    }
    println!(
        "  peak during batch : +{:.3} MiB above baseline",
        mib(peak() - baseline)
    );
    println!(
        "  LIVE after batch  : {:+.3} MiB vs baseline (retained)",
        mib(live() - before_batch)
    );

    // ── second batch: monotonic growth = leak; flat = steady transient ───────
    let before_batch2 = live();
    for _ in 0..30 {
        let r = engine.run(&query).unwrap();
        drop(r);
    }
    println!(
        "  LIVE after 2nd batch: {:+.3} MiB vs after 1st (monotonic ⇒ leak)",
        mib(live() - before_batch2)
    );

    // ── CONCURRENT burst: the real per-mini-VM worst case (max 3 tenants on one DB).
    //    Measures whether the engine serves NEAREST in parallel (peak ≈ C×32 MiB) or
    //    serialises (peak ≈ 32 MiB) — the number that decides if a 128 MB tier is viable.
    println!("\n-- concurrent burst (real threads) — per-mini-VM worst case --");
    for c in [1usize, 3] {
        reset_peak_to_live();
        let before = live();
        std::thread::scope(|s| {
            let hs: Vec<_> = (0..c)
                .map(|_| {
                    s.spawn(|| {
                        let r = engine.run(&query).unwrap();
                        std::hint::black_box(&r);
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
        });
        println!(
            "  {c} concurrent: transient peak +{:.1} MiB above baseline",
            mib(peak() - before)
        );
    }
    println!(
        "\n  resident floor (allocator-live, lower bound on RSS; cgroup anon ≈ 48 MiB/agent) = {:.0} MiB",
        mib(baseline)
    );
    println!(
        "  → 128 MB tier viability = resident + 3-concurrent transient vs 128; column cuts the transient 4×"
    );

    println!("\n-- read --");
    println!(
        "  bucket-size spike (~{:.0} MiB) ⇒ materialisation ⇒ COLUMN fixes balloon+latency",
        mib(N as i64 * 4096)
    );
    println!(
        "  small spike + retained≈0 ⇒ allocator scratch ⇒ arena / return-to-OS (no column needed)"
    );
    println!("  retained grows per batch ⇒ genuine retention ⇒ find the held allocation\n");
}
