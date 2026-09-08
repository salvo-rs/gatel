use gatel_cgi_protocol::environment::Environment;
use gatel_cgi_protocol::{Error, scgi};

fn env(length: &[u8]) -> Environment {
    let mut env = Environment::new(4096);
    env.insert("CONTENT_LENGTH", length).unwrap();
    env
}

#[test]
fn published_header_vector_is_exact() {
    let mut env = env(b"27");
    env.insert("REQUEST_URI", b"/deepthought").unwrap();
    env.insert("REQUEST_METHOD", b"POST").unwrap();
    assert_eq!(
        scgi::headers(&env, 27, 70).unwrap(),
        b"70:CONTENT_LENGTH\x0027\x00SCGI\x001\x00REQUEST_METHOD\x00POST\x00REQUEST_URI\x00/deepthought\x00,"
    );
    assert_eq!(scgi::headers(&env, 27, 69), Err(Error::Limit));
}

#[test]
fn length_is_required_numeric_and_exact() {
    assert_eq!(
        scgi::headers(&Environment::new(64), 0, 64),
        Err(Error::InvalidEnvironment)
    );
    for invalid in [b"".as_slice(), b"+1", b" 1", b"1 ", b"-1"] {
        assert_eq!(
            scgi::headers(&env(invalid), 1, 64),
            Err(Error::InvalidEnvironment)
        );
    }
    assert_eq!(
        scgi::headers(&env(b"18446744073709551616"), 0, 64),
        Err(Error::BodyLength)
    );
    assert_eq!(scgi::headers(&env(b"1"), 2, 64), Err(Error::BodyLength));
    // Leading zeros are legal in CONTENT_LENGTH, but never in netstring length.
    assert!(scgi::headers(&env(b"00"), 0, 64).unwrap().starts_with(b"25:"));
}

#[test]
fn marker_is_inserted_once_and_cannot_be_overridden() {
    let mut env = env(b"0");
    let without = scgi::headers(&env, 0, 64).unwrap();
    assert_eq!(without, b"24:CONTENT_LENGTH\x000\x00SCGI\x001\x00,");
    env.insert("SCGI", b"1").unwrap();
    assert_eq!(scgi::headers(&env, 0, 64).unwrap(), without);
    let mut invalid = Environment::new(64);
    invalid.insert("CONTENT_LENGTH", b"0").unwrap();
    invalid.insert("SCGI", b"2").unwrap();
    assert_eq!(scgi::headers(&invalid, 0, 64), Err(Error::InvalidEnvironment));
}

#[test]
fn wire_budget_includes_injected_marker_and_counts_bytes() {
    let mut env = env(b"0");
    env.insert("A", "你好".as_bytes()).unwrap();
    let wire = scgi::headers(&env, 0, 33).unwrap();
    assert!(wire.starts_with(b"33:"));
    assert_eq!(wire.len(), 37);
    assert_eq!(scgi::headers(&env, 0, 32), Err(Error::Limit));
}
