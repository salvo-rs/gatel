use std::io;
use std::time::Duration;

use gatel_cgi_protocol::environment::Environment;
use gatel_cgi_protocol::head::Head;
use gatel_scgi::exchange::{self, Error, Limits, Output};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Sink {
    body: Vec<u8>,
    finished: bool,
    heads: usize,
}
impl Output for Sink {
    async fn head(&mut self, _: Head) -> io::Result<()> {
        self.heads += 1;
        Ok(())
    }
    async fn body(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.body.extend(bytes);
        Ok(())
    }
    async fn finish(&mut self) -> io::Result<()> {
        self.finished = true;
        Ok(())
    }
}
fn env(length: u64) -> Environment {
    let mut env = Environment::new(1024);
    env.insert("CONTENT_LENGTH", length.to_string().as_bytes())
        .unwrap();
    env
}

#[tokio::test]
async fn real_tcp_backend_observes_half_close_and_fragmented_response() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let backend = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            // This completes only if upload half-closes; response remains writable.
            socket.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"24:CONTENT_LENGTH\x005\x00SCGI\x001\x00,hello");
            for byte in b"Content-Type: text/plain\r\nContent-Length: 5\r\n\r\nworld" {
                socket.write_all(&[*byte]).await.unwrap();
            }
            socket.shutdown().await.unwrap();
        });
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut sink = Sink::default();
        exchange::run(
            socket,
            b"hello".as_slice(),
            &env(5),
            5,
            false,
            &mut sink,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(sink.body, b"world");
        assert!(sink.finished);
        backend.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn external_cancellation_drops_pending_input_and_socket() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let (input, mut producer) = tokio::io::duplex(1);
    let mut sink = Sink::default();
    let environment = env(100);
    let mut running = Box::pin(exchange::run(
        client,
        input,
        &environment,
        100,
        false,
        &mut sink,
        Limits::default(),
    ));
    let mut prefix = [0; 3];
    tokio::select! {
        result = peer.read_exact(&mut prefix) => { result.unwrap(); },
        result = &mut running => panic!("exchange unexpectedly completed: {result:?}"),
    }
    drop(running);
    assert!(!sink.finished);
    assert!(producer.write_all(b"x").await.is_err());
    assert!(peer.write_all(b"x").await.is_err());
    let mut remainder = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut remainder))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn stalled_output_callback_times_out_without_finish() {
    struct Blocked;
    impl Output for Blocked {
        async fn head(&mut self, _: Head) -> io::Result<()> {
            Ok(())
        }
        async fn body(&mut self, _: &[u8]) -> io::Result<()> {
            std::future::pending().await
        }
        async fn finish(&mut self) -> io::Result<()> {
            panic!("must not finish a stalled response")
        }
    }
    let (client, mut peer) = tokio::io::duplex(1024);
    peer.write_all(b"Content-Type: text/plain\nContent-Length: 2\n\nok")
        .await
        .unwrap();
    let limits = Limits {
        idle: Duration::from_secs(1),
        overall: Duration::from_secs(30),
        ..Limits::default()
    };
    let start = tokio::time::Instant::now();
    assert!(matches!(
        exchange::run(client, b"".as_slice(), &env(0), 0, false, &mut Blocked, limits).await,
        Err(Error::Timeout)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(1));
    assert!(peer.write_all(b"x").await.is_err());
}

#[tokio::test]
async fn bidirectional_small_buffers_stream_large_request_and_response() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, mut peer) = tokio::io::duplex(32);
        let backend = tokio::spawn(async move {
            let mut digits = Vec::new();
            loop {
                let b = peer.read_u8().await.unwrap();
                if b == b':' {
                    break;
                }
                digits.push(b);
            }
            let length: usize = std::str::from_utf8(&digits).unwrap().parse().unwrap();
            let mut headers = vec![0; length];
            peer.read_exact(&mut headers).await.unwrap();
            assert_eq!(peer.read_u8().await.unwrap(), b',');
            assert_eq!(headers, b"CONTENT_LENGTH\x00100000\x00SCGI\x001\x00");
            let (mut reader, mut writer) = tokio::io::split(peer);
            let upload = async {
                let mut body = Vec::new();
                reader.read_to_end(&mut body).await.unwrap();
                assert_eq!(body, vec![b'x'; 100000]);
            };
            let response = async {
                writer
                    .write_all(b"Content-Type: text/plain\nContent-Length: 100000\n\n")
                    .await
                    .unwrap();
                writer.write_all(&vec![b'y'; 100000]).await.unwrap();
                // Keep output open until all input is received, below.
            };
            tokio::join!(upload, response);
            writer.shutdown().await.unwrap();
        });
        let mut sink = Sink::default();
        exchange::run(
            client,
            std::io::Cursor::new(vec![b'x'; 100000]),
            &env(100000),
            100000,
            false,
            &mut sink,
            Limits::default(),
        )
        .await
        .unwrap();
        backend.await.unwrap();
        assert_eq!(sink.body, vec![b'y'; 100000]);
        assert_eq!(sink.heads, 1);
        assert!(sink.finished);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn truncated_overlong_and_head_bodies_never_finish() {
    for (body, head) in [(b"ab".as_slice(), false), (b"abcd", false), (b"abc", true)] {
        let (client, mut peer) = tokio::io::duplex(1024);
        let response = [
            b"Content-Type: text/plain\nContent-Length: 3\n\n".as_slice(),
            body,
        ]
        .concat();
        peer.write_all(&response).await.unwrap();
        peer.shutdown().await.unwrap();
        let mut sink = Sink::default();
        assert!(
            exchange::run(
                client,
                b"".as_slice(),
                &env(0),
                0,
                head,
                &mut sink,
                Limits::default()
            )
            .await
            .is_err()
        );
        assert!(!sink.finished);
        assert!(sink.body.len() < 3);
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_closes_owned_transport() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let mut sink = Sink::default();
    let limits = Limits {
        overall: Duration::from_secs(1),
        ..Limits::default()
    };
    assert!(matches!(
        exchange::run(client, b"".as_slice(), &env(0), 0, false, &mut sink, limits).await,
        Err(Error::Timeout)
    ));
    assert!(!sink.finished);
    assert!(peer.write_all(b"late response").await.is_err());
}

#[tokio::test]
async fn early_complete_response_cancels_pending_input() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let (input, mut producer) = tokio::io::duplex(1);
    peer.write_all(b"Status: 403 Forbidden\nContent-Type: text/plain\nContent-Length: 0\n\n")
        .await
        .unwrap();
    peer.shutdown().await.unwrap();
    let mut sink = Sink::default();
    tokio::time::timeout(
        Duration::from_secs(1),
        exchange::run(client, input, &env(100), 100, false, &mut sink, Limits::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(sink.finished);
    assert!(producer.write_all(b"x").await.is_err());
}

#[tokio::test]
async fn short_and_excess_input_fail_and_close_transport() {
    for input in [b"ab".as_slice(), b"abcd"] {
        let (client, mut peer) = tokio::io::duplex(1024);
        let mut sink = Sink::default();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            exchange::run(client, input, &env(3), 3, false, &mut sink, Limits::default()),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(Error::RequestLength)));
        assert!(!sink.finished);
        assert!(peer.write_all(b"late").await.is_err());
        let mut wire = Vec::new();
        peer.read_to_end(&mut wire).await.unwrap();
        let expected_body = if input.len() < 3 { input } else { &input[..3] };
        let expected = [
            b"24:CONTENT_LENGTH\x003\x00SCGI\x001\x00,".as_slice(),
            expected_body,
        ]
        .concat();
        assert_eq!(wire, expected, "excess bytes must never reach the backend");
    }
}

#[tokio::test]
async fn declared_budget_failure_sends_nothing() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let mut sink = Sink::default();
    let limits = Limits {
        max_request: 2,
        ..Limits::default()
    };
    assert!(matches!(
        exchange::run(client, b"abc".as_slice(), &env(3), 3, false, &mut sink, limits).await,
        Err(Error::RequestLength)
    ));
    let mut wire = Vec::new();
    peer.read_to_end(&mut wire).await.unwrap();
    assert!(wire.is_empty());
    assert!(!sink.finished);
}
