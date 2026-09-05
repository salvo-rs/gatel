//! The KDL frontend, given arbitrary bytes.
//!
//! This is the layer that turns a file into nodes. It must reject anything it
//! cannot understand, with a position, and it must never panic: a panic here
//! is a gateway that dies on a reload, which is the failure the whole
//! transaction design exists to prevent.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // Either nodes or diagnostics. Never a panic, and never a hang.
    let _ = gatel_kdl::lower(text, "fuzz.kdl");
});
