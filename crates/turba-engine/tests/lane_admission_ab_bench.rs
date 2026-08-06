//! v0.4 cp 4.2.2: A/B microbenchmark for the BlockCache lane admission
//! policy.
//!
//! ## Workload shape
//!
//! - Cache capacity: 32 MiB (production-scale; below this `quick_cache
//!   0.6 with_weighter(items_estimate=10_000, …)` admission becomes
//!   erratic in synthetic tests due to a per-item-budget mismatch — see
//!   the bench notes below).
//! - User hot set: 4 unique blocks of 64 KiB, pre-warmed via
//!   `UserIORead`. All start resident in cache.
//! - Read mix: alternates `UserIORead` on the hot set (round-robin) +
//!   `Compaction` reads on a fresh key per iteration. The compaction
//!   reads simulate a k-way merge sweeping through SSTs; their keys
//!   are distinct so each is a fresh miss.
//! - Iterations: 1 500 user + 1 500 compaction (= 96 MiB compaction
//!   churn through a 32 MiB cache, ~3× capacity).
//!
//! ## Empirical finding (this run, 2026-05-09)
//!
//! Under quick_cache 0.6's S3-FIFO eviction policy, the hot user set
//! stays at 100 % hit rate in BOTH policy modes. S3-FIFO's small/main
//! queue separation already protects continuously-accessed user blocks
//! from cold compaction-read evictions. The lane-aware admission policy
//! is operationally REDUNDANT in this configuration: the policy
//! correctly skips compaction admits (verified via the admission
//! counters), but the downstream user-hit-rate benefit it would
//! produce against a less-sophisticated cache (LRU, FIFO) does not
//! materialise here.
//!
//! Per cycle plan §3 Bloque 4.2.2 R4.2 (improvement < 10 %): the
//! policy stays implemented (plumbing, stats, tests), is registered
//! as finding **H7** for v0.5 sub-cycle refinement, and the server
//! flag `--block-cache-lane-admission` is configured **default
//! disabled** rather than the originally-planned enabled.
//!
//! ## What this bench asserts
//!
//! - Policy correctness: with policy ON, compaction misses produce
//!   `skipped++` (zero `admitted`); with policy OFF, every compaction
//!   miss produces `admitted++` (zero `skipped`). This validates the
//!   admission decision regardless of downstream hit-rate.
//! - The cycle plan acceptance gate (`≥ 10 % hit rate improvement`)
//!   is NOT asserted — the bench reports the measured delta as
//!   evidence for the H7 finding.

// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use turba_engine::cache::{BlockCache, BlockHandle, DecodedBlock};
use turba_engine::io::Lane;

const BLOCK_BYTES: usize = 64 * 1024; // 64 KiB
// Cache size + iteration count tuned so the compaction churn meaningfully
// pressures the cache. quick_cache 0.6's internal `with_weighter`
// admission becomes erratic at small (cap, items_estimate) ratios; the
// production BlockCache::new uses items_estimate=10_000 so we stay
// near production scale here. 32 MiB / 64 KiB = 512 blocks; 1500
// compaction iterations exceed cap by ~3× → measurable churn.
const CACHE_CAPACITY_BYTES: u64 = 32 * 1024 * 1024;
const HOT_SET_SIZE: u64 = 4; // user hot blocks
const ITERATIONS: u64 = 1500; // 1500 user + 1500 compaction reads

fn make_block(byte: u8) -> DecodedBlock {
    DecodedBlock {
        data: vec![byte; BLOCK_BYTES],
    }
}

fn user_handle(i: u64) -> BlockHandle {
    BlockHandle {
        tree_id: 1,
        table_id: 1,
        offset: i * BLOCK_BYTES as u64,
    }
}

fn compaction_handle(i: u64) -> BlockHandle {
    BlockHandle {
        tree_id: 1,
        table_id: 99, // distinct table_id keeps the keyspace disjoint
        offset: i * BLOCK_BYTES as u64,
    }
}

#[derive(Debug, Default)]
struct RunStats {
    user_reads: u64,
    user_hits: u64,
    user_misses: u64,
    compaction_reads: u64,
    compaction_hits: u64,
    compaction_misses: u64,
    admission: [(u64, u64); Lane::COUNT],
}

impl RunStats {
    fn user_hit_rate(&self) -> f64 {
        if self.user_reads == 0 {
            return 0.0;
        }
        self.user_hits as f64 / self.user_reads as f64
    }
}

fn run_one(policy_enabled: bool) -> RunStats {
    let cache = BlockCache::with_config(CACHE_CAPACITY_BYTES, policy_enabled);

    // Warm the user hot set into the "hot" tier of quick_cache's S3-FIFO
    // policy. A single insert + immediate read does not promote a block
    // out of the small/probationary queue; repeated reads do. We do
    // WARMUP_ROUNDS rounds of reading each block so that the hot set
    // is genuinely promoted to the main queue before the alternating
    // user+compaction phase begins.
    const WARMUP_ROUNDS: usize = 20;
    for _round in 0..WARMUP_ROUNDS {
        for i in 0..HOT_SET_SIZE {
            let _ = cache
                .get_or_load(user_handle(i), Lane::UserIORead, || Ok(make_block(i as u8)))
                .expect("preload");
        }
    }

    // Counters: closure firing == miss. The closure captures these.
    let user_miss_count = Arc::new(AtomicU64::new(0));
    let compaction_miss_count = Arc::new(AtomicU64::new(0));

    for i in 0..ITERATIONS {
        // User read on the hot set (round-robin).
        {
            let umc = Arc::clone(&user_miss_count);
            let h = user_handle(i % HOT_SET_SIZE);
            let _ = cache
                .get_or_load(h, Lane::UserIORead, || {
                    umc.fetch_add(1, Ordering::Relaxed);
                    Ok(make_block(0xaa))
                })
                .expect("user read");
        }
        // Compaction read on a fresh key.
        {
            let cmc = Arc::clone(&compaction_miss_count);
            let h = compaction_handle(i);
            let _ = cache
                .get_or_load(h, Lane::Compaction { target_level: 1 }, || {
                    cmc.fetch_add(1, Ordering::Relaxed);
                    Ok(make_block(0xcc))
                })
                .expect("compaction read");
        }
    }

    let user_misses = user_miss_count.load(Ordering::Relaxed);
    let compaction_misses = compaction_miss_count.load(Ordering::Relaxed);
    RunStats {
        user_reads: ITERATIONS,
        user_hits: ITERATIONS - user_misses,
        user_misses,
        compaction_reads: ITERATIONS,
        compaction_hits: ITERATIONS - compaction_misses,
        compaction_misses,
        admission: cache.admission_snapshot(),
    }
}

#[test]
fn lane_admission_ab_bench() {
    eprintln!("v0.4 cp 4.2.2 — BlockCache lane admission A/B");
    eprintln!(
        "  cache_capacity = {} bytes; hot_set = {} blocks × {} B; iterations = {}",
        CACHE_CAPACITY_BYTES, HOT_SET_SIZE, BLOCK_BYTES, ITERATIONS
    );

    let off = run_one(/*policy_enabled=*/ false);
    let on = run_one(/*policy_enabled=*/ true);

    let off_rate = off.user_hit_rate();
    let on_rate = on.user_hit_rate();
    let abs_delta = on_rate - off_rate;
    let rel_delta_pct = if off_rate > 0.0 {
        (on_rate - off_rate) / off_rate * 100.0
    } else if on_rate > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };

    eprintln!();
    eprintln!("policy OFF (--block-cache-lane-admission disabled):");
    eprintln!(
        "  user_reads={} user_hits={} user_misses={} → hit_rate={:.4}",
        off.user_reads, off.user_hits, off.user_misses, off_rate
    );
    eprintln!(
        "  compaction_reads={} compaction_hits={} compaction_misses={}",
        off.compaction_reads, off.compaction_hits, off.compaction_misses
    );
    eprintln!(
        "  admission: user_io_read={:?} compaction={:?} flush={:?} writer_durable={:?}",
        off.admission[0], off.admission[3], off.admission[2], off.admission[1]
    );

    eprintln!();
    eprintln!("policy ON  (--block-cache-lane-admission enabled, default):");
    eprintln!(
        "  user_reads={} user_hits={} user_misses={} → hit_rate={:.4}",
        on.user_reads, on.user_hits, on.user_misses, on_rate
    );
    eprintln!(
        "  compaction_reads={} compaction_hits={} compaction_misses={}",
        on.compaction_reads, on.compaction_hits, on.compaction_misses
    );
    eprintln!(
        "  admission: user_io_read={:?} compaction={:?} flush={:?} writer_durable={:?}",
        on.admission[0], on.admission[3], on.admission[2], on.admission[1]
    );

    eprintln!();
    eprintln!(
        "delta: user-side hit rate {:.4} → {:.4} (abs {:+.4}, rel {:+.2}%)",
        off_rate, on_rate, abs_delta, rel_delta_pct
    );

    // POLICY CORRECTNESS asserts (always required):
    // - Compaction admissions counter behaviour under each mode.
    //   Compaction lane = index 3.
    assert_eq!(
        on.admission[3].1, ITERATIONS,
        "policy ON: every compaction miss must skip admission"
    );
    assert_eq!(
        on.admission[3].0, 0,
        "policy ON: compaction admitted_total must stay 0"
    );
    assert_eq!(
        off.admission[3].1, 0,
        "policy OFF: skipped_total must stay 0"
    );
    assert_eq!(
        off.admission[3].0, ITERATIONS,
        "policy OFF: every compaction miss must admit"
    );
    // - User reads always admit on miss regardless of policy.
    //   UserIORead lane = index 0.
    assert_eq!(off.admission[0].1, 0, "user reads never skip");
    assert_eq!(on.admission[0].1, 0, "user reads never skip");

    // Hit-rate delta is REPORTED, not asserted. The cycle plan §3
    // Bloque 4.2.2 acceptance gate of ≥ 10 % improvement is not met in
    // this microbench because quick_cache 0.6's S3-FIFO already
    // protects the hot user set without help from the lane admission
    // policy (see file-level rustdoc + finding H7 in cycle plan §8).
    eprintln!(
        "\nNote: hit-rate improvement {:+.2} pp does not meet the cycle plan \
         §3 Bloque 4.2.2 ≥ 10 % gate; see finding H7. Policy is implemented \
         + stats are operative; flag defaults DISABLED in v0.4. Refinement \
         deferred to v0.5 sub-cycle (richer workload or different cache \
         eviction policy where the admission control matters).",
        abs_delta * 100.0
    );
}
