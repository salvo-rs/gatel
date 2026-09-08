#![no_main]
#[path = "../../plugins/decode/tests/support/fuzz_cases.rs"]
mod fuzz_cases;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        })
        .block_on(fuzz_cases::exercise(data));
});
