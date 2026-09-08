#![no_main]
use libfuzzer_sys::fuzz_target;
#[path = "../../crates/gatel-cgi-protocol/tests/support/fuzz_cases.rs"]
mod cases;
fuzz_target!(|data: &[u8]| cases::exercise(data));
