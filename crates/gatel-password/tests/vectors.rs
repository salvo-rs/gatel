//! Known answers generated independently of RustCrypto, not round-trip-only tests.
use gatel_password::Credential;

// PHP 8.4.25, php@sha256:59fa733c9af643a122f8a9976119460e35ce76dd0a3f2b9c8f75af8e361a54e2.
// password_hash(" fixture ", PASSWORD_ARGON2ID,
//   ["memory_cost"=>19456,"time_cost"=>2,"threads"=>1]); generated 2026-09-07.
const PHP_ARGON2ID: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$aVQ3TkRzbDF5VG8yRUdLYg$z27DCmz6vAC079OH+W4nyOLKZh8/rZXeiv07Tw+AQLc";

#[test]
fn independently_generated_php_hash_respects_compiled_capability() {
    let credential = Credential::parse(PHP_ARGON2ID);
    #[cfg(feature = "argon2")]
    {
        let credential = credential.unwrap();
        assert!(credential.verify(b" fixture "));
        assert!(!credential.verify(b"fixture"));
        assert!(!credential.verify(b" fixture !"));
    }
    #[cfg(not(feature = "argon2"))]
    assert!(credential.is_err());
}

#[cfg(feature = "argon2")]
#[test]
fn complete_parameter_matrix_is_checked_without_hashing() {
    for memory in [8192, 19456, 32768] {
        for iterations in [1, 2, 4] {
            for lanes in [1, 2, 4] {
                let hash =
                    PHP_ARGON2ID.replace("m=19456,t=2,p=1", &format!("m={memory},t={iterations},p={lanes}"));
                assert!(Credential::parse(&hash).is_ok());
                assert_eq!(Credential::parse(&hash).unwrap().memory_bytes(), memory * 1024);
                // A well-formed configuration is not proof that a password matches.
                // Do not compute all of these high-end parameter combinations.
            }
        }
    }
    for parameters in [
        "m=8191,t=2,p=1",
        "m=32769,t=2,p=1",
        "m=4294967295,t=2,p=1",
        "m=4294967296,t=2,p=1",
        "m=19456,t=0,p=1",
        "m=19456,t=5,p=1",
        "m=19456,t=4294967295,p=1",
        "m=19456,t=2,p=0",
        "m=19456,t=2,p=5",
        "m=19456,t=2,p=4294967295",
        "m=19456,t=2",
        "m=19456,p=1",
        "t=2,p=1",
        "m=19456,t=2,p=1,p=1",
        "m=19456,t=2,p=1,data=YQ",
        "m=19456,t=2,p=1,keyid=YQ",
    ] {
        assert!(
            Credential::parse(&PHP_ARGON2ID.replace("m=19456,t=2,p=1", parameters)).is_err(),
            "{parameters}"
        );
    }
}

#[cfg(feature = "argon2")]
#[test]
fn salt_digest_and_encoding_boundaries_do_not_bypass_policy() {
    use argon2::password_hash::phc::{Output, PasswordHash, Salt};
    for size in [8, 15, 17, 48] {
        let mut hash = PasswordHash::new(PHP_ARGON2ID).unwrap();
        hash.salt = Some(Salt::new(&vec![0; size]).unwrap());
        assert!(Credential::parse(&hash.to_string()).is_err());
    }
    for size in [16, 31, 33, 64] {
        let mut hash = PasswordHash::new(PHP_ARGON2ID).unwrap();
        hash.hash = Some(Output::new(&vec![0; size]).unwrap());
        assert!(Credential::parse(&hash.to_string()).is_err());
    }
    for invalid in [
        PHP_ARGON2ID.replace("$v=19", ""),
        PHP_ARGON2ID.replace("v=19", "v=16"),
        PHP_ARGON2ID.replace("argon2id", "argon2d"),
        format!("{PHP_ARGON2ID}\n"),
        format!("{PHP_ARGON2ID}\0"),
        format!("{PHP_ARGON2ID}$extra"),
        "x".repeat(257),
    ] {
        assert!(Credential::parse(&invalid).is_err());
    }
}
