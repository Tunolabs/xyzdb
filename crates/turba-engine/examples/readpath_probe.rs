//! Read-path DIRECTION probe for 0.9 Fase 1 (Mac / ARM). This is NOT a
//! benchmark and emits NO publishable magnitude: it reports allocation
//! accounting (exact, deterministic, hardware-independent) plus a coarse
//! wall-time, isolating the mechanism each read-path change exploits. The
//! real x86 magnitude (perf/heaptrack, AVX2) is deferred to the AWS close
//! block per the work order.
//!
//! Two modes, both operate directly on `turba_engine::tree::Tree` — the layer
//! where the changes live — so no query grammar or deserialize confounds them:
//!
//! * `g4`  — peak live bytes of `range` (eager `Vec<Entry>`) vs `range_stream`
//!           (lazy) over one flushed key range. This is exactly the source
//!           materialization the 8 converted callsites now avoid.
//! * `g1a` — wall-time + total bytes allocated for one COLD full scan (fresh
//!           cache ⇒ every block a miss ⇒ `load_block` decode path). Run it
//!           before and after the G1a edit (git stash) to see the miss-path
//!           delta: one decode per miss instead of two.
//!
//! Usage: `cargo run --release --example readpath_probe -- [g4|g1a] [N]`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

// ─── Counting global allocator ───────────────────────────────────────────────
// Single-threaded workload ⇒ the peak update needs no strict ordering; the CAS
// loop keeps it monotone. `live` is bytes currently held; `peak` is the
// high-water since the last `reset`; `total` is cumulative allocated bytes.

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static BASE: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: delegates every allocation to the system allocator; the atomics never
// allocate, so there is no re-entrancy.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: forwarded verbatim to the system allocator (see impl-level note).
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let cur = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            TOTAL.fetch_add(l.size(), Relaxed);
            let mut pk = PEAK.load(Relaxed);
            while cur > pk {
                match PEAK.compare_exchange_weak(pk, cur, Relaxed, Relaxed) {
                    Ok(_) => break,
                    Err(x) => pk = x,
                }
            }
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: forwarded verbatim to the system allocator (see impl-level note).
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Anchor `peak`/`base` at the current live level and zero the cumulative counter.
fn reset() {
    let live = LIVE.load(Relaxed);
    BASE.store(live, Relaxed);
    PEAK.store(live, Relaxed);
    TOTAL.store(0, Relaxed);
}
/// Peak live bytes reached above the level at the last `reset` (independent of
/// what is still live when this is read — measures the high-water of the phase).
fn peak_delta() -> usize {
    PEAK.load(Relaxed).saturating_sub(BASE.load(Relaxed))
}
fn total() -> usize {
    TOTAL.load(Relaxed)
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn tree_config() -> TreeConfig {
    TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::Lz4,
            data_block_size: 4096,
            ..Default::default()
        },
        max_memtable_size: 64 * 1024,
        compaction: LeveledConfig::default(),
        level_compressions: None,
    }
}

/// Build a tree at `path`, insert `n` entries with 256-byte values, seal+flush
/// so the whole range lives in SSTables (the read source), then drop the handle
/// to release the dir lock. Keys are `k{i:06}` so `[k000000, k999999]` spans all.
fn build_flushed(path: &std::path::Path, n: usize) {
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let tree = Tree::open(path, tree_config(), cache).unwrap();
    let val = vec![0x5au8; 256];
    for i in 0..n {
        let k = format!("k{i:06}").into_bytes();
        tree.insert(&k, &val).unwrap();
    }
    assert!(tree.seal_active());
    tree.flush_sealed().unwrap();
    // Any residual active memtable → also seal so the read source is disk-only.
    if tree.seal_active() {
        tree.flush_sealed().unwrap();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "g4".to_string());
    let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20_000);

    let root = std::env::temp_dir().join(format!("readpath_probe_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("tree");
    build_flushed(&path, n);

    let (lo, hi) = (b"k000000".to_vec(), b"k999999".to_vec());

    match mode.as_str() {
        "g4" => {
            // Warm the cache once so the decode/cache-fill cost is paid before
            // the comparison — this isolates the SOURCE materialization delta.
            let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
            let tree = Tree::open(&path, tree_config(), cache).unwrap();
            let warm = tree.range_stream(&lo, &hi).unwrap().count();

            reset();
            let v = tree.range(&lo, &hi).unwrap();
            let cnt_range = v.len();
            let peak_range = peak_delta();
            std::hint::black_box(&v);
            drop(v);

            reset();
            let cnt_stream = tree.range_stream(&lo, &hi).unwrap().count();
            let peak_stream = peak_delta();

            assert_eq!(warm, cnt_range, "warm vs range count mismatch");
            assert_eq!(
                cnt_range, cnt_stream,
                "range vs range_stream count mismatch"
            );

            println!("── G4 direction probe (Tree source materialization) ──");
            println!("  entries scanned      : {cnt_range}");
            println!(
                "  range()        peak  : {:>9.3} MiB   (eager Vec<Entry>)",
                mib(peak_range)
            );
            println!(
                "  range_stream() peak  : {:>9.3} MiB   (lazy)",
                mib(peak_stream)
            );
            let factor = if peak_stream > 0 {
                peak_range as f64 / peak_stream as f64
            } else {
                f64::INFINITY
            };
            println!(
                "  peak reduction       : {:>9.3} MiB   ({factor:.1}× smaller)",
                mib(peak_range.saturating_sub(peak_stream))
            );
            println!(
                "  → direction: source RAM O(bucket) → O(block). Bit-identical (range ≡ range_stream().collect())."
            );
        }
        "g1a" => {
            // Cold read: a FRESH cache ⇒ every block is a first-touch miss ⇒
            // the `load_block` decode path fires for each. This is the path G1a
            // changed (one decode per miss instead of two).
            let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
            let tree = Tree::open(&path, tree_config(), cache).unwrap();

            reset();
            let t0 = Instant::now();
            let cnt = tree.range_stream(&lo, &hi).unwrap().count();
            let elapsed = t0.elapsed();
            let total_alloc = total();

            println!("── G1a direction probe (cold miss decode path) ──");
            println!("  entries scanned      : {cnt}");
            println!(
                "  cold full scan time  : {:>9.3} ms   (Mac/ARM, direction only)",
                elapsed.as_secs_f64() * 1e3
            );
            println!(
                "  total bytes allocated: {:>9.3} MiB   (decode-path work proxy)",
                mib(total_alloc)
            );
            println!(
                "  → compare before/after G1a (git stash the 2 lib files): fewer decodes per miss."
            );
        }
        other => {
            eprintln!("unknown mode {other:?}; use g4 or g1a");
            std::process::exit(2);
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}
