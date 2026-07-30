//! loom model of an ArcSwap-style atomic pointer swap under concurrent reader.
//!
//! Phase 0 scaffolding: minimal harness that compiles under `RUSTFLAGS="--cfg loom"`
//! and exercises the swap-vs-read interleaving in loom's model checker. It does NOT
//! yet wrap the real `SuperVersion` — doing so requires routing production `Arc` /
//! `AtomicPtr` through `loom::sync` under `cfg(loom)` in `turba-engine`, which is
//! Phase 7 work.
//!
//! Run with:
//! ```
//! RUSTFLAGS="--cfg loom" cargo test --test supversion_loom --release
//! ```
//!
//! Without the cfg flag, this file compiles to a no-op test — the default
//! `cargo test` run stays fast.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;

#[test]
fn atomic_pointer_swap_no_torn_reads() {
    loom::model(|| {
        let version = Arc::new(AtomicUsize::new(1));

        let reader = {
            let v = version.clone();
            thread::spawn(move || {
                let observed = v.load(Ordering::Acquire);
                assert!(
                    observed == 1 || observed == 2,
                    "unexpected version {observed}"
                );
            })
        };

        let writer = {
            let v = version.clone();
            thread::spawn(move || {
                v.store(2, Ordering::Release);
            })
        };

        reader.join().unwrap();
        writer.join().unwrap();

        let final_ = version.load(Ordering::SeqCst);
        assert_eq!(final_, 2);
    });
}
