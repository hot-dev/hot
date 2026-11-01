//! The formatter never panics, and formatting its own output changes nothing.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    // An Err is the formatter refusing input it can't format safely.
    if let Ok(formatted) = hot::lang::fmt::format_str(source, 4) {
        let again = hot::lang::fmt::format_str(&formatted, 4)
            .expect("the formatter rejected its own output");
        assert_eq!(formatted, again, "formatting is not idempotent");
    }
});
