/// Temporary diagnostic: count raw entries in spatial tree by lobe_id prefix.
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::tree::{Tree, TreeConfig};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: count_spatial <path_to_bench_dir>");
        std::process::exit(1);
    });

    let spatial_path = Path::new(&path).join("spatial");
    eprintln!("Opening spatial tree at: {}", spatial_path.display());

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let config = TreeConfig::default();

    let tree = Tree::open(&spatial_path, config, cache).expect("failed to open spatial tree");

    eprintln!("Counting entries...");
    let mut counts: HashMap<u16, u64> = HashMap::new();
    let mut total: u64 = 0;

    for entry in tree.scan_all().expect("scan_all failed") {
        total += 1;
        if entry.key.len() >= 2 {
            let lobe_id = u16::from_be_bytes([entry.key[0], entry.key[1]]);
            *counts.entry(lobe_id).or_insert(0) += 1;
        }
        if total % 5_000_000 == 0 {
            eprintln!("  ... {total} entries scanned");
        }
    }

    println!("Total entries in spatial: {total}");
    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by_key(|(id, _)| *id);
    for (lobe_id, count) in &sorted {
        println!("  lobe_id={lobe_id}: {count}");
    }
}
