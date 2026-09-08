use gatel_cgi_protocol::environment::Environment;
use gatel_cgi_protocol::head::Decoder;
use gatel_cgi_protocol::{Error, fastcgi as fcgi};

#[test]
fn environment_is_explicit_bounded_and_debug_redacted() {
    let mut env = Environment::new(100);
    env.insert("HTTP_AUTHORIZATION", b"secret-token").unwrap();
    assert!(!format!("{env:?}").contains("secret-token"));
    assert_eq!(
        env.insert("HTTP_AUTHORIZATION", b"override"),
        Err(Error::DuplicateEnvironment)
    );
    for name in ["", "9NAME", "a", "A=B", "A\0B", "A-B"] {
        assert_eq!(env.insert(name, b"x"), Err(Error::InvalidEnvironment));
    }
    assert_eq!(env.insert("VALUE", b"nul\0value"), Err(Error::InvalidEnvironment));
    assert_eq!(env.insert("LONG", &[b'x'; 100]), Err(Error::Limit));
    assert_eq!(env.iter().count(), 1, "failed insertion must be atomic");
}

#[test]
fn every_split_preserves_header_and_body_boundary() {
    let head = b"Status: 201 Created\r\nContent-Type: text/plain\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 4\r\n\r\n";
    for split in 0..head.len() {
        let mut decoder = Decoder::new(head.len());
        assert!(decoder.feed(&head[..split]).unwrap().1.is_none());
        let mut rest = head[split..].to_vec();
        rest.extend_from_slice(&[42; 100_000]);
        let (used, parsed) = decoder.feed(&rest).unwrap();
        let parsed = parsed.unwrap();
        assert_eq!(used, head.len() - split);
        assert_eq!(parsed.status, 201);
        assert_eq!(parsed.content_length, Some(4));
        assert_eq!(parsed.headers.get_all("set-cookie").iter().count(), 2);
        assert!(!parsed.headers.contains_key("status"));
        assert!(decoder.eof().is_ok());
        assert_eq!(decoder.feed(b"more").unwrap_err(), Error::StreamClosed);
    }
}

#[test]
fn malformed_headers_are_rejected() {
    let mut bad = Decoder::new(128);
    assert!(bad.feed(b"Content-Type: \x0btext/plain\n\n").is_err());
    assert!(bad.eof().is_err());
    for input in [
        "Status: 101 Switching\n\n",
        "Status: 200\nStatus: 201\n\n",
        "Content-Type: x\nContent-Length: 1\nContent-Length: 1\n\n",
        "Content-Type: x\nContent-Length: +1\n\n",
        "Content-Type: x\nTransfer-Encoding: chunked\n\n",
        "Content-Type: x\nConnection: x-private\n\n",
        "Content-Type: x\n folded: value\n\n",
        "Content-Type : x\n\n",
        "Content-Type: x\rBAD: y\n\n",
        "HTTP/1.1 200 OK\n\n",
        "Content-Type: x\nLocation: /a\nLocation: /b\n\n",
        "Content-Type: x\nX-Bad: nul\0byte\n\n",
    ] {
        assert!(
            Decoder::new(4096).feed(input.as_bytes()).is_err(),
            "accepted {input:?}"
        );
    }
    assert_eq!(Decoder::new(4).feed(b"xxxxx").unwrap_err(), Error::Limit);
    let mut incomplete = Decoder::new(50);
    incomplete.feed(b"Content-Type: x\n").unwrap();
    assert_eq!(incomplete.eof(), Err(Error::IncompleteResponse));
}

#[test]
fn redirect_profile_is_explicit() {
    let (_, head) = Decoder::new(512)
        .feed(b"Location: https://example.com/\n\n")
        .unwrap();
    assert_eq!(head.unwrap().status, 302);
    assert_eq!(
        Decoder::new(512).feed(b"Location: /internal\n\n").unwrap_err(),
        Error::UnsupportedLocalRedirect
    );
    assert_eq!(
        Decoder::new(512)
            .feed(b"Status: 204 No Content\n\n")
            .unwrap()
            .1
            .unwrap()
            .status,
        204
    );
}

#[test]
fn body_length_rejects_truncation_overrun_and_head_payloads() {
    let head = Decoder::new(512)
        .feed(b"Content-Type: text/plain\nContent-Length: 4\n\n")
        .unwrap()
        .1
        .unwrap();
    let mut length = gatel_cgi_protocol::head::BodyLength::new(&head, false);
    length.observe(3).unwrap();
    assert_eq!(length.finish(), Err(Error::BodyLength));
    assert_eq!(length.observe(2), Err(Error::BodyLength));
    length.observe(1).unwrap();
    length.finish().unwrap();
    let mut head_length = gatel_cgi_protocol::head::BodyLength::new(&head, true);
    head_length.finish().unwrap();
    assert_eq!(head_length.observe(1), Err(Error::BodyLength));
    assert!(
        Decoder::new(512)
            .feed(b"Status: 204 No Content\nContent-Length: 0\n\n")
            .is_err()
    );
}

#[test]
fn fixed_fastcgi_vectors_and_padding() {
    assert_eq!(
        fcgi::begin(),
        vec![1, 1, 0, 1, 0, 8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]
    );
    let wire = [1, 6, 0, 1, 0, 3, 2, 0, b'a', b'b', b'c', 9, 9];
    for split in 0..wire.len() {
        assert!(fcgi::decode(&wire[..split]).unwrap().is_none());
    }
    let (record, used) = fcgi::decode(&wire).unwrap().unwrap();
    assert_eq!(used, wire.len());
    assert_eq!(record.body, b"abc");
    assert_eq!(record.request, 1);
    assert_eq!(
        fcgi::decode(&[2, 6, 0, 1, 0, 0, 0, 0]).unwrap_err(),
        Error::InvalidRecord
    );
    assert_eq!(fcgi::record(5, &[0; 65536]), Err(Error::Limit));
}

#[test]
fn params_long_lengths_and_cross_record_fragmentation() {
    let mut env = Environment::new(200_000);
    env.insert("A", &vec![b'x'; 70_000]).unwrap();
    let wire = fcgi::params(&env).unwrap();
    let mut offset = 0;
    let mut stream = Vec::new();
    let mut lengths = Vec::new();
    while offset < wire.len() {
        let (record, used) = fcgi::decode(&wire[offset..]).unwrap().unwrap();
        assert_eq!(record.kind, fcgi::PARAMS);
        lengths.push(record.body.len());
        stream.extend_from_slice(record.body);
        offset += used;
    }
    assert_eq!(lengths, vec![65535, 4471, 0]);
    assert_eq!(&stream[..6], &[1, 0x80, 1, 0x11, 0x70, b'A']);
    assert_eq!(stream.len(), 70006);
}

fn record(kind: u8, body: &[u8]) -> fcgi::Record<'_> {
    fcgi::Record {
        kind,
        request: 1,
        body,
    }
}

#[test]
fn responder_requires_closed_stdout_and_end_request() {
    let mut response = fcgi::Responder::new(8);
    assert_eq!(response.eof(), Err(Error::IncompleteResponse));
    response.accept(record(6, b"Content-Type: x\n\nbody")).unwrap();
    response.accept(record(6, b"")).unwrap();
    assert_eq!(response.accept(record(6, b"late")), Err(Error::StreamClosed));
    response.accept(record(7, b"warning")).unwrap();
    response.accept(record(7, b"")).unwrap();
    assert_eq!(
        response.accept(record(3, &[0; 8])).unwrap(),
        fcgi::Event::Complete { app_status: 0 }
    );
    assert!(response.eof().is_ok());
    assert_eq!(response.accept(record(3, &[0; 8])), Err(Error::StreamClosed));
}

#[test]
fn php_fpm_implicit_stdout_close_still_requires_end_request() {
    let mut response = fcgi::Responder::new(10);
    response
        .accept(record(6, b"Content-Type: text/plain\n\nbody"))
        .unwrap();
    assert_eq!(response.eof(), Err(Error::IncompleteResponse));
    assert_eq!(
        response.accept(record(3, &[0; 8])).unwrap(),
        fcgi::Event::Complete { app_status: 0 }
    );
    assert!(response.eof().is_ok());
    assert_eq!(response.accept(record(6, b"late")), Err(Error::StreamClosed));
}

#[test]
fn responder_refuses_foreign_requests_and_stderr_floods() {
    let mut response = fcgi::Responder::new(4);
    assert_eq!(
        response.accept(fcgi::Record {
            kind: 6,
            request: 2,
            body: b"x"
        }),
        Err(Error::UnexpectedRecord)
    );
    assert_eq!(response.accept(record(7, b"12345")), Err(Error::Limit));
    assert_eq!(response.accept(record(9, b"")), Err(Error::UnexpectedRecord));
    assert_eq!(response.accept(record(3, &[0; 7])), Err(Error::InvalidRecord));
    assert_eq!(
        response.accept(record(3, &[0, 0, 0, 0, 2, 0, 0, 0])),
        Err(Error::BackendRejected)
    );
}
