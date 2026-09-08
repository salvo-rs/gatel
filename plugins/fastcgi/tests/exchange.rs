use std::io;
use std::time::Duration;

use gatel_cgi_protocol::environment::Environment;
use gatel_cgi_protocol::head::Head;
use gatel_fastcgi::exchange::{self, Error, Limits, Output};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

#[derive(Default)]
struct Collected {
    status: Option<u16>,
    body: Vec<u8>,
    finished: bool,
}
impl Output for Collected {
    async fn head(&mut self, head: Head) -> io::Result<()> {
        self.status = Some(head.status.as_u16());
        Ok(())
    }
    async fn body(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.body.extend_from_slice(bytes);
        Ok(())
    }
    async fn finish(&mut self) -> io::Result<()> {
        self.finished = true;
        Ok(())
    }
}

fn environment(length: usize) -> Environment {
    let mut env = Environment::new(1024);
    env.insert("CONTENT_LENGTH", length.to_string().as_bytes())
        .unwrap();
    env
}

#[tokio::test]
#[ignore = "requires explicit GATEL_PHP_FPM_ADDR with /app/index.php fixture"]
async fn real_php_fpm_transport() {
    let stream = tokio::net::TcpStream::connect(std::env::var("GATEL_PHP_FPM_ADDR").unwrap())
        .await
        .unwrap();
    let mut env = environment(0);
    for (name, value) in [
        ("SCRIPT_FILENAME", "/app/index.php"),
        ("REQUEST_METHOD", "GET"),
        ("GATEWAY_INTERFACE", "CGI/1.1"),
        ("SERVER_PROTOCOL", "HTTP/1.1"),
        ("SCRIPT_NAME", "/index.php"),
    ] {
        env.insert(name, value.as_bytes()).unwrap();
    }
    let mut output = Collected::default();
    let result = exchange::run(stream, &b""[..], &env, 0, false, &mut output, Limits::default()).await;
    assert!(
        result.is_ok(),
        "result={result:?}, status={:?}, bytes={}",
        output.status,
        output.body.len()
    );
    assert_eq!(output.status, Some(200));
    assert!(output.body.windows(18).any(|part| part == b"Hello from FastCGI"));
    assert!(output.finished);
}

#[tokio::test(start_paused = true)]
async fn progressing_upload_can_outlast_the_initial_response_idle_budget() {
    let (client, mut backend) = tokio::io::duplex(256);
    let (input, mut upload) = tokio::io::duplex(1);
    let producer = tokio::spawn(async move {
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            upload.write_all(b"x").await.unwrap();
        }
    });
    let peer = tokio::spawn(async move {
        params(&mut backend).await;
        loop {
            let (_, body) = read_record(&mut backend).await;
            if body.is_empty() {
                break;
            }
        }
        write_record(
            &mut backend,
            6,
            b"Content-Type: text/plain\nContent-Length: 0\n\n",
        )
        .await;
        write_record(&mut backend, 6, b"").await;
        write_record(&mut backend, 3, &[0; 8]).await;
    });
    let mut output = Collected::default();
    exchange::run(
        client,
        input,
        &environment(3),
        3,
        false,
        &mut output,
        Limits {
            idle: Duration::from_secs(2),
            overall: Duration::from_secs(10),
            ..Limits::default()
        },
    )
    .await
    .unwrap();
    producer.await.unwrap();
    peer.await.unwrap();
    assert!(output.finished);
}

struct BlockedOutput;
impl Output for BlockedOutput {
    async fn head(&mut self, _: Head) -> io::Result<()> {
        Ok(())
    }
    async fn body(&mut self, _: &[u8]) -> io::Result<()> {
        std::future::pending().await
    }
    async fn finish(&mut self) -> io::Result<()> {
        panic!("blocked output marked successful")
    }
}

#[tokio::test(start_paused = true)]
async fn downstream_backpressure_is_bounded_without_detached_workers() {
    let (client, mut backend) = tokio::io::duplex(256);
    let peer = tokio::spawn(async move {
        params(&mut backend).await;
        let _ = read_record(&mut backend).await;
        write_record(&mut backend, 6, b"Content-Type: text/plain\n\ndata").await;
        let mut remainder = Vec::new();
        backend.read_to_end(&mut remainder).await.unwrap();
    });
    let result = exchange::run(
        client,
        &b""[..],
        &environment(0),
        0,
        false,
        &mut BlockedOutput,
        Limits {
            idle: Duration::from_secs(1),
            ..Limits::default()
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Timeout)));
    peer.await.unwrap();
}

async fn write_record(stream: &mut DuplexStream, kind: u8, body: &[u8]) {
    let len = (body.len() as u16).to_be_bytes();
    stream
        .write_all(&[1, kind, 0, 1, len[0], len[1], 0, 0])
        .await
        .unwrap();
    stream.write_all(body).await.unwrap();
}

async fn read_record(stream: &mut DuplexStream) -> (u8, Vec<u8>) {
    let mut prefix = [0; 8];
    stream.read_exact(&mut prefix).await.unwrap();
    assert_eq!(prefix[0], 1);
    assert_eq!(&prefix[2..4], &[0, 1]);
    let mut body = vec![0; u16::from_be_bytes([prefix[4], prefix[5]]) as usize];
    stream.read_exact(&mut body).await.unwrap();
    let mut padding = vec![0; prefix[6] as usize];
    stream.read_exact(&mut padding).await.unwrap();
    (prefix[1], body)
}

async fn params(stream: &mut DuplexStream) {
    let (kind, role) = read_record(stream).await;
    assert_eq!(kind, 1);
    assert_eq!(role, [0, 1, 0, 0, 0, 0, 0, 0]);
    loop {
        let (kind, bytes) = read_record(stream).await;
        assert_eq!(kind, 4);
        if bytes.is_empty() {
            break;
        }
    }
}

#[tokio::test]
async fn response_and_upload_progress_concurrently_over_small_buffers() {
    let (client, mut backend) = tokio::io::duplex(32);
    let bytes = vec![b'x'; 100_000];
    let env = environment(bytes.len());
    let peer = tokio::spawn(async move {
        params(&mut backend).await;
        // The server writes a response before consuming stdin. A sequential
        // upload-then-read implementation deadlocks on these 32-byte buffers.
        write_record(
            &mut backend,
            6,
            b"Content-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
        )
        .await;
        let mut uploaded = 0;
        loop {
            let (kind, bytes) = read_record(&mut backend).await;
            assert_eq!(kind, 5);
            if bytes.is_empty() {
                break;
            }
            uploaded += bytes.len();
        }
        assert_eq!(uploaded, 100_000);
        write_record(&mut backend, 7, b"diagnostic not forwarded").await;
        write_record(&mut backend, 6, b"").await;
        write_record(&mut backend, 3, &[0; 8]).await;
    });
    let mut output = Collected::default();
    let report = exchange::run(
        client,
        bytes.as_slice(),
        &env,
        bytes.len() as u64,
        false,
        &mut output,
        Limits {
            overall: Duration::from_secs(2),
            ..Limits::default()
        },
    )
    .await
    .unwrap();
    peer.await.unwrap();
    assert_eq!(output.status, Some(200));
    assert_eq!(output.body, b"hello");
    assert!(output.finished);
    assert_eq!(report.stderr_bytes, 24);
}

#[tokio::test]
async fn early_response_cancels_stalled_client_upload() {
    let (client, mut backend) = tokio::io::duplex(32);
    let (input, input_sender) = tokio::io::duplex(1);
    let peer = tokio::spawn(async move {
        params(&mut backend).await;
        write_record(
            &mut backend,
            6,
            b"Status: 403 Denied\nContent-Type: text/plain\nContent-Length: 0\n\n",
        )
        .await;
        write_record(&mut backend, 6, b"").await;
        write_record(&mut backend, 3, &[0; 8]).await;
    });
    let mut output = Collected::default();
    exchange::run(
        client,
        input,
        &environment(10),
        10,
        false,
        &mut output,
        Limits {
            overall: Duration::from_secs(1),
            ..Limits::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(output.status, Some(403));
    assert!(output.finished);
    drop(input_sender);
    peer.await.unwrap();
}

#[tokio::test]
async fn incomplete_or_nonzero_exit_responses_never_finish_successfully() {
    for app_status in [0u8, 7] {
        let (client, mut backend) = tokio::io::duplex(256);
        let peer = tokio::spawn(async move {
            params(&mut backend).await;
            let _ = read_record(&mut backend).await;
            write_record(
                &mut backend,
                6,
                b"Content-Type: text/plain\nContent-Length: 5\n\nhello",
            )
            .await;
            write_record(&mut backend, 6, b"").await;
            if app_status != 0 {
                write_record(&mut backend, 3, &[0, 0, 0, app_status, 0, 0, 0, 0]).await;
            }
        });
        let mut output = Collected::default();
        let result = exchange::run(
            client,
            &b""[..],
            &environment(0),
            0,
            false,
            &mut output,
            Limits::default(),
        )
        .await;
        assert!(result.is_err());
        assert!(!output.finished);
        peer.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_backend_obeys_deadline_and_drops_the_transport() {
    let (client, mut backend) = tokio::io::duplex(4096);
    let mut output = Collected::default();
    let result = exchange::run(
        client,
        &b""[..],
        &environment(0),
        0,
        false,
        &mut output,
        Limits {
            idle: Duration::from_secs(2),
            overall: Duration::from_secs(3),
            ..Limits::default()
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Timeout)));
    let mut sent = Vec::new();
    backend.read_to_end(&mut sent).await.unwrap();
    assert!(!sent.is_empty());
    assert!(!output.finished);
}

#[tokio::test]
async fn cancelling_exchange_drops_both_transport_and_input() {
    let (client, mut backend) = tokio::io::duplex(4096);
    let (input, mut input_sender) = tokio::io::duplex(1);
    let mut output = Collected::default();
    let env = environment(10);
    let mut exchange = Box::pin(exchange::run(
        client,
        input,
        &env,
        10,
        false,
        &mut output,
        Limits::default(),
    ));
    tokio::select! { _ = &mut exchange => panic!("unexpected completion"), _ = params(&mut backend) => {} }
    drop(exchange);
    assert!(input_sender.write_all(b"x").await.is_err());
    let mut rest = Vec::new();
    backend.read_to_end(&mut rest).await.unwrap();
    assert!(!output.finished);
}

#[tokio::test]
async fn mismatched_environment_and_short_body_fail() {
    let (client, _backend) = tokio::io::duplex(4096);
    let mut output = Collected::default();
    assert!(matches!(
        exchange::run(
            client,
            &b""[..],
            &environment(1),
            0,
            false,
            &mut output,
            Limits::default()
        )
        .await,
        Err(Error::RequestLength)
    ));
    let (client, _backend) = tokio::io::duplex(4096);
    assert!(matches!(
        exchange::run(
            client,
            &b"a"[..],
            &environment(2),
            2,
            false,
            &mut output,
            Limits::default()
        )
        .await,
        Err(Error::RequestLength)
    ));
}
