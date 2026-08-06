#![no_main]
// SPDX-License-Identifier: BUSL-1.1
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = turba_engine::block::decode(data);
    let _ = turba_engine::block::validate_checksum(data);
});
