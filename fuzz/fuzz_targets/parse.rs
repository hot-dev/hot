//! Any input either parses or is a located parse error: never a panic, a
//! stack overflow or a hang.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        let _ = hot::lang::parser::parse_hot_file(source, "fuzz.hot");
    }
});
