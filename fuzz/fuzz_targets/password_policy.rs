#![no_main]
use libfuzzer_sys::fuzz_target;
#[path = "../../crates/gatel-password/tests/support/fuzz_cases.rs"]
mod cases;
fuzz_target!(|data: &[u8]| {
    cases::exercise(data);
    // Keep valid encodings reachable even before a corpus has been populated.
    for seed in cases::SEEDS {
        cases::exercise(seed);
    }
});
