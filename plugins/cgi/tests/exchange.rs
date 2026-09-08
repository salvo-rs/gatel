use std::future::Future;
use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::task::Context;
use std::time::Duration;

use gatel_cgi::exchange;
use gatel_cgi::exchange::{Error, Limits, Output};
use gatel_cgi_protocol::head::Head;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn status(success: bool) -> ExitStatus {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(if success { 0 } else { 256 })
}
#[derive(Default)]
struct Seen {
    heads: usize,
    body: Vec<u8>,
    finished: bool,
}
#[derive(Default)]
struct Capture(Arc<Mutex<Seen>>);
impl Output for Capture {
    async fn head(&mut self, _: Head) -> io::Result<()> {
        self.0.lock().unwrap().heads += 1;
        Ok(())
    }
    async fn body(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().body.extend(bytes);
        Ok(())
    }
    async fn finish(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().finished = true;
        Ok(())
    }
}
const RESPONSE: &[u8] = b"Content-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";

#[tokio::test]
async fn stdout_eof_does_not_release_final_byte_before_successful_exit() {
    for success in [true, false] {
        let mut output = Capture::default();
        let seen = output.0.clone();
        let (release, exit) = tokio::sync::oneshot::channel();
        let mut running = Box::pin(exchange::run(
            tokio::io::sink(),
            RESPONSE,
            tokio::io::empty(),
            tokio::io::empty(),
            async { Ok(exit.await.unwrap()) },
            0,
            false,
            &mut output,
            Limits::default(),
        ));
        assert!(
            running
                .as_mut()
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        assert_eq!(seen.lock().unwrap().body, b"hell");
        assert!(!seen.lock().unwrap().finished);
        release.send(status(success)).unwrap();
        let result = running.await;
        if success {
            assert_eq!(result.unwrap(), 0);
            assert_eq!(seen.lock().unwrap().body, b"hello");
            assert!(seen.lock().unwrap().finished);
        } else {
            assert!(matches!(result, Err(Error::ExitFailure)));
            assert_eq!(seen.lock().unwrap().body, b"hell");
            assert!(!seen.lock().unwrap().finished);
        }
    }
}

#[tokio::test]
async fn tiny_pipes_allow_concurrent_upload_stdout_and_stderr() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (stdin, mut input_peer) = tokio::io::duplex(32);
        let (stdout, mut output_peer) = tokio::io::duplex(32);
        let (stderr, mut error_peer) = tokio::io::duplex(32);
        let peer = tokio::spawn(async move {
            let upload = async {
                let mut bytes = Vec::new();
                input_peer.read_to_end(&mut bytes).await.unwrap();
                assert_eq!(bytes, vec![b'x'; 100_000]);
            };
            let download = async {
                output_peer
                    .write_all(b"Content-Type: text/plain\nContent-Length: 100000\n\n")
                    .await
                    .unwrap();
                output_peer.write_all(&vec![b'y'; 100_000]).await.unwrap();
                output_peer.shutdown().await.unwrap();
            };
            let errors = async {
                error_peer.write_all(&[b'e'; 512]).await.unwrap();
                error_peer.shutdown().await.unwrap();
            };
            tokio::join!(upload, download, errors);
            status(true)
        });
        let mut output = Capture::default();
        let result = exchange::run(
            stdin,
            stdout,
            stderr,
            std::io::Cursor::new(vec![b'x'; 100_000]),
            async { Ok(peer.await.unwrap()) },
            100_000,
            false,
            &mut output,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(result, 512);
        let seen = output.0.lock().unwrap();
        assert_eq!(seen.heads, 1);
        assert_eq!(seen.body, vec![b'y'; 100_000]);
        assert!(seen.finished);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn stderr_limit_and_invalid_body_never_finish_the_response() {
    for (response, stderr, head, expected_stderr) in [
        (RESPONSE, &b"secret"[..], false, true),
        (&RESPONSE[..RESPONSE.len() - 1], &b""[..], false, false),
        (RESPONSE, &b""[..], true, false),
    ] {
        let mut output = Capture::default();
        let result = exchange::run(
            tokio::io::sink(),
            response,
            stderr,
            tokio::io::empty(),
            async { Ok(status(true)) },
            0,
            head,
            &mut output,
            Limits {
                max_stderr: 5,
                ..Limits::default()
            },
        )
        .await;
        if expected_stderr {
            assert!(matches!(result, Err(Error::StderrLimit)));
        } else {
            assert!(matches!(result, Err(Error::Protocol(_))));
        }
        assert!(!output.0.lock().unwrap().finished);
    }
}

#[tokio::test(start_paused = true)]
async fn quiet_stderr_is_not_an_idle_failure_but_exit_is_bounded_by_overall_time() {
    let (stderr, writer) = tokio::io::duplex(32);
    let mut output = Capture::default();
    exchange::run(
        tokio::io::sink(),
        RESPONSE,
        stderr,
        tokio::io::empty(),
        async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            drop(writer);
            Ok(status(true))
        },
        0,
        false,
        &mut output,
        Limits {
            idle: Duration::from_secs(1),
            ..Limits::default()
        },
    )
    .await
    .unwrap();
    let mut output = Capture::default();
    let result = exchange::run(
        tokio::io::sink(),
        RESPONSE,
        tokio::io::empty(),
        tokio::io::empty(),
        std::future::pending::<io::Result<ExitStatus>>(),
        0,
        false,
        &mut output,
        Limits {
            overall: Duration::from_secs(1),
            ..Limits::default()
        },
    )
    .await;
    assert!(matches!(result, Err(Error::Timeout)));
    assert_eq!(output.0.lock().unwrap().body, b"hell");
    assert!(!output.0.lock().unwrap().finished);
}

#[tokio::test]
async fn cancellation_drops_all_pipes_input_and_exit_wait_without_finishing() {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    struct PendingInput(Guard);
    impl tokio::io::AsyncRead for PendingInput {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let _ = &self.0;
            Poll::Pending
        }
    }
    let input_dropped = Arc::new(AtomicBool::new(false));
    let exit_dropped = Arc::new(AtomicBool::new(false));
    let exit_guard = Guard(exit_dropped.clone());
    let (stdin, mut input_peer) = tokio::io::duplex(32);
    let (stdout, mut output_peer) = tokio::io::duplex(32);
    let (stderr, mut error_peer) = tokio::io::duplex(32);
    let mut output = Capture::default();
    let mut running = Box::pin(exchange::run(
        stdin,
        stdout,
        stderr,
        PendingInput(Guard(input_dropped.clone())),
        async move {
            let _guard = exit_guard;
            std::future::pending::<io::Result<ExitStatus>>().await
        },
        1,
        false,
        &mut output,
        Limits::default(),
    ));
    assert!(
        running
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    assert!(!input_dropped.load(Ordering::SeqCst));
    drop(running);
    assert!(input_dropped.load(Ordering::SeqCst));
    assert!(exit_dropped.load(Ordering::SeqCst));
    assert_eq!(input_peer.read(&mut [0; 1]).await.unwrap(), 0);
    assert!(output_peer.write_all(b"x").await.is_err());
    assert!(error_peer.write_all(b"x").await.is_err());
    assert!(!output.0.lock().unwrap().finished);
}

#[tokio::test]
async fn upload_length_is_exact_unless_a_complete_response_cancels_upload() {
    for (input, length) in [(&b""[..], 1), (&b"extra"[..], 0)] {
        let (stdout, _hold) = tokio::io::duplex(32);
        let mut output = Capture::default();
        let result = exchange::run(
            tokio::io::sink(),
            stdout,
            tokio::io::empty(),
            input,
            std::future::pending::<io::Result<ExitStatus>>(),
            length,
            false,
            &mut output,
            Limits::default(),
        )
        .await;
        assert!(matches!(result, Err(Error::RequestLength)));
        assert!(!output.0.lock().unwrap().finished);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Head,
    Body,
    Tail,
    Finish,
}
struct InterruptedOutput {
    stage: Stage,
    stall: bool,
    finish_completed: bool,
}
impl InterruptedOutput {
    async fn check(&self, stage: Stage) -> io::Result<()> {
        if self.stage != stage {
            return Ok(());
        }
        if self.stall {
            std::future::pending().await
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "private sink detail"))
        }
    }
}
impl Output for InterruptedOutput {
    async fn head(&mut self, _: Head) -> io::Result<()> {
        self.check(Stage::Head).await
    }
    async fn body(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.check(if bytes == b"o" { Stage::Tail } else { Stage::Body })
            .await
    }
    async fn finish(&mut self) -> io::Result<()> {
        self.check(Stage::Finish).await?;
        self.finish_completed = true;
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn output_backpressure_and_errors_are_bounded_at_every_stage() {
    for stage in [Stage::Head, Stage::Body, Stage::Tail, Stage::Finish] {
        for stall in [false, true] {
            for overall_first in [false, true] {
                let mut output = InterruptedOutput {
                    stage,
                    stall,
                    finish_completed: false,
                };
                let start = tokio::time::Instant::now();
                let result = exchange::run(
                    tokio::io::sink(),
                    RESPONSE,
                    tokio::io::empty(),
                    tokio::io::empty(),
                    async { Ok(status(true)) },
                    0,
                    false,
                    &mut output,
                    Limits {
                        overall: Duration::from_secs(if overall_first { 1 } else { 10 }),
                        idle: Duration::from_secs(2),
                        ..Limits::default()
                    },
                )
                .await;
                if stall {
                    assert!(matches!(result, Err(Error::Timeout)));
                    assert_eq!(
                        start.elapsed(),
                        Duration::from_secs(if overall_first { 1 } else { 2 })
                    );
                } else {
                    let error = result.unwrap_err();
                    assert!(matches!(&error, Error::Io(e) if e.kind() == io::ErrorKind::BrokenPipe));
                    assert!(!error.to_string().contains("private sink detail"));
                }
                assert!(!output.finish_completed);
            }
        }
    }
}

#[tokio::test]
async fn stderr_eof_is_required_even_after_successful_exit() {
    let (stderr, mut writer) = tokio::io::duplex(32);
    let mut output = Capture::default();
    let seen = output.0.clone();
    let mut running = Box::pin(exchange::run(
        tokio::io::sink(),
        RESPONSE,
        stderr,
        tokio::io::empty(),
        async { Ok(status(true)) },
        0,
        false,
        &mut output,
        Limits::default(),
    ));
    assert!(
        running
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    assert_eq!(seen.lock().unwrap().body, b"hell");
    assert!(!seen.lock().unwrap().finished);
    writer.write_all(b"diagnostic").await.unwrap();
    writer.shutdown().await.unwrap();
    assert_eq!(running.await.unwrap(), 10);
    assert_eq!(seen.lock().unwrap().body, b"hello");
    assert!(seen.lock().unwrap().finished);
}
