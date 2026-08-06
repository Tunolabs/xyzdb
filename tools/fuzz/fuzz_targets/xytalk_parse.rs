#![no_main]
// SPDX-License-Identifier: BUSL-1.1
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = xytalk_parser::parse(s);
        let _ = xytalk_parser::parse_multi(s);
    }
});
