#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = turba_engine::journal::entry::decode_batches(data);
});
