#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = turba_engine::block::decode(data);
    let _ = turba_engine::block::validate_checksum(data);
});
