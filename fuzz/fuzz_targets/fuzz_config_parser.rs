#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    // Ensure the parser doesn't panic on arbitrary input.
    let _ = gatel_core::config::parse_config(data);
});
