use gatel_cgi_protocol::environment::Environment;
use gatel_cgi_protocol::head::{Decoder, Head};
use gatel_cgi_protocol::{Error, fastcgi, scgi};

pub fn exercise(data: &[u8]) {
    if data.len() > 128 * 1024 {
        return;
    }
    let Some((&mode, input)) = data.split_first() else {
        return;
    };
    match mode % 3 {
        0 => {
            let whole = parse(input, input.len().max(1));
            let split = parse(input, (mode as usize % 31) + 1);
            match (whole, split) {
                (Ok(Some((a, left))), Ok(Some((b, right)))) => {
                    assert_eq!(a, b);
                    assert_eq!(left.status, right.status);
                    assert_eq!(left.headers, right.headers);
                    assert_eq!(left.content_length, right.content_length);
                }
                (Ok(None), Ok(None)) => {}
                (Err(a), Err(b)) => assert_eq!(a, b),
                _ => panic!("CGI head result depends on input fragmentation"),
            }
        }
        1 => {
            let mut rest = input;
            let mut responder = fastcgi::Responder::new(4096);
            while let Ok(Some((record, used))) = fastcgi::decode(rest) {
                assert!(used >= 8 && used <= rest.len());
                if responder.accept(record).is_err() {
                    break;
                }
                rest = &rest[used..];
            }
            let _ = responder.eof();
            // Exercise state transitions even when random input has no valid wire prefix.
            let mut responder = fastcgi::Responder::new(256);
            for chunk in input.chunks(16) {
                let kind = [fastcgi::STDOUT, fastcgi::STDERR, fastcgi::END][chunk[0] as usize % 3];
                let _ = responder.accept(fastcgi::Record {
                    kind,
                    request: 1,
                    body: &chunk[1..],
                });
            }
        }
        _ => {
            let mut env = Environment::new(128 * 1024);
            env.insert("CONTENT_LENGTH", b"0").unwrap();
            // Long values exercise four-byte lengths and PARAMS record boundaries.
            let _ = env.insert("VALUE", input);
            for (index, value) in input.chunks(32).enumerate() {
                if env.insert(&format!("X_{index}"), value).is_err() {
                    break;
                }
            }
            let wire = scgi::headers(&env, 0, 256 * 1024).unwrap();
            let colon = wire.iter().position(|b| *b == b':').unwrap();
            let size: usize = std::str::from_utf8(&wire[..colon]).unwrap().parse().unwrap();
            assert_eq!(wire.len(), colon + size + 2);
            assert_eq!(wire.last(), Some(&b','));
            assert!(wire[colon + 1..].starts_with(b"CONTENT_LENGTH\x000\x00SCGI\x001\x00"));
            let mut decoded = std::collections::BTreeMap::new();
            let mut fields = wire[colon + 1..colon + size].split(|b| *b == 0);
            while let Some(name) = fields.next() {
                let value = fields.next().unwrap();
                assert!(decoded.insert(name, value).is_none());
            }
            assert_eq!(decoded.remove(b"SCGI".as_slice()), Some(b"1".as_slice()));
            let expected: std::collections::BTreeMap<_, _> =
                env.iter().map(|(name, value)| (name.as_bytes(), value)).collect();
            assert_eq!(decoded, expected);
            let params = fastcgi::params(&env).unwrap();
            let mut rest = params.as_slice();
            let mut payload = Vec::new();
            let mut terminated = false;
            while !rest.is_empty() {
                let (record, used) = fastcgi::decode(rest).unwrap().unwrap();
                assert!(!terminated);
                assert_eq!(record.kind, fastcgi::PARAMS);
                assert_eq!(record.request, fastcgi::REQUEST);
                terminated = record.body.is_empty();
                payload.extend_from_slice(record.body);
                rest = &rest[used..];
            }
            assert!(terminated);
            let mut rest = payload.as_slice();
            let mut decoded = std::collections::BTreeMap::new();
            while !rest.is_empty() {
                let name_len = parameter_length(&mut rest);
                let value_len = parameter_length(&mut rest);
                let (name, tail) = rest.split_at(name_len);
                let (value, tail) = tail.split_at(value_len);
                assert!(decoded.insert(name, value).is_none());
                rest = tail;
            }
            assert_eq!(decoded, expected);
        }
    }
}

// Independent test-side decoding checks the encoder rather than reusing it.
fn parameter_length(input: &mut &[u8]) -> usize {
    let first = input[0];
    if first & 0x80 == 0 {
        *input = &input[1..];
        first as usize
    } else {
        let value = u32::from_be_bytes(input[..4].try_into().unwrap()) & 0x7fff_ffff;
        *input = &input[4..];
        assert!(value >= 128);
        value as usize
    }
}

fn parse(input: &[u8], step: usize) -> Result<Option<(usize, Head)>, Error> {
    let mut decoder = Decoder::new(4096);
    let mut consumed = 0;
    for chunk in input.chunks(step) {
        let (used, head) = decoder.feed(chunk)?;
        assert!(used <= chunk.len());
        consumed += used;
        if let Some(head) = head {
            return Ok(Some((consumed, head)));
        }
    }
    Ok(None)
}
