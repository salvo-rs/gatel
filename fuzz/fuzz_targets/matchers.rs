//! The parsers a request reaches before anything else.
//!
//! A host, a path pattern and a secret reference. The first two are compared
//! against attacker-chosen text on every request; the third decides which file
//! a configuration is allowed to read, so a parse that disagrees with the
//! containment check is a way out of the configuration directory.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };

    if let Ok(host) = gatel_plan::HostMatch::parse(text) {
        // Whatever it parsed to has to answer about itself without panicking,
        // and has to survive being asked about the text it came from.
        let _ = host.describe();
        let _ = host.local_name();
        let _ = host.specificity();
        let _ = host.matches(text);
    }

    if let Ok(path) = gatel_plan::PathMatch::parse(text) {
        let _ = path.describe();
        let _ = path.specificity();
        let _ = path.matches(text);
    }

    if let Ok(reference) = gatel_model::SecretRef::parse(text) {
        // `display` is the redacted form. It is what an error message would
        // carry, so it must never come back holding the material.
        let _ = reference.display();
    }

    let _ = gatel_plan::Target::parse(text);
});
