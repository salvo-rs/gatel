#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    // Ensure the duration parser doesn't panic on arbitrary input.
    // The parse_duration function is internal to config::parse, so we
    // test it indirectly through a config that exercises duration parsing.
    let config = format!(
        r#"global {{
    grace-period "{data}"
}}
site "*" {{
    route "/*" {{
        respond "ok" status=200
    }}
}}"#
    );
    let _ = gatel_core::config::parse_config(&config);
});
