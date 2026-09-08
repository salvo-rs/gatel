//! Shared parser-only invariants: never run attacker-selected password work.
use gatel_password::Credential;

pub const SEEDS: &[&[u8]] = &[
    b"",
    // Canonical synthetic bcrypt encoding: parser seed, not a password fixture.
    b"$2b$04$.....................................................",
    b"$2b$31$invalid",
    b"$argon2id$v=19$m=4294967295,t=4294967295,p=4294967295$invalid$invalid",
    b"$argon2id$v=19$m=19456,t=2,p=1$aVQ3TkRzbDF5VG8yRUdLYg$z27DCmz6vAC079OH+W4nyOLKZh8/rZXeiv07Tw+AQLc",
];

pub fn exercise(data: &[u8]) {
    let Ok(encoded) = std::str::from_utf8(data) else {
        return;
    };
    let parsed = Credential::parse(encoded);
    assert_eq!(parsed.is_ok(), Credential::parse(encoded).is_ok());
    match parsed {
        Ok(credential) => {
            assert!(data.len() <= 256);
            assert_eq!(format!("{credential:?}"), "Credential(<redacted>)");
        }
        Err(error) => {
            // Errors must never incorporate the encoded credential.
            assert!(matches!(
                error,
                "unsupported, malformed or over-budget password hash"
                    | "unsupported, malformed or over-budget password hash (only bcrypt enabled)"
            ));
        }
    }
}
