#[path = "support/fuzz_cases.rs"]
mod cases;

#[test]
fn every_byte_mutation_and_truncation_preserves_parser_invariants() {
    assert!(gatel_password::Credential::parse(std::str::from_utf8(cases::SEEDS[1]).unwrap()).is_ok());
    for seed in cases::SEEDS {
        for length in 0..=seed.len() {
            cases::exercise(&seed[..length]);
        }
        for offset in 0..seed.len() {
            for byte in 0..=255 {
                let mut changed = seed.to_vec();
                changed[offset] = byte;
                cases::exercise(&changed);
            }
        }
    }
    cases::exercise(&vec![b'a'; 65536]);
}
