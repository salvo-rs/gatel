// A harness-free integration binary doubles as the real CGI executable: no
// test runner preamble can corrupt its CGI response stream.
#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("cgi-fixture") {
        fixture();
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(verify());
}

#[cfg(windows)]
fn fixture() {
    use std::io::{Read, Write};
    let mode = std::env::var("CGI_MODE").unwrap();
    if mode == "hang" {
        print!("Content-Type: text/plain\r\nContent-Length: 5\r\n\r\nhell");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(30));
        return;
    }
    if mode == "stderr" {
        eprint!("excessive stderr");
    }
    let mut body = Vec::new();
    std::io::stdin().read_to_end(&mut body).unwrap();
    print!(
        "Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    std::io::stdout().write_all(&body).unwrap();
    std::io::stdout().flush().unwrap();
    std::process::exit(if mode == "fail" { 7 } else { 0 });
}

#[cfg(windows)]
async fn verify() {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use gatel_cgi::exchange::{Error, Limits, Output};
    use gatel_cgi::job::{Invocation, Program, Request};
    use gatel_cgi::supervisor;
    use gatel_cgi_protocol::environment::Environment;
    use gatel_cgi_protocol::head::Head;
    use gatel_plugin_runtime::ShutdownSignal;
    use tokio::time::{Instant, timeout};
    #[derive(Default)]
    struct Seen {
        heads: usize,
        body: Vec<u8>,
        finished: bool,
    }
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
    let program = Arc::new(Program {
        executable: std::env::current_exe().unwrap(),
        arguments: vec!["cgi-fixture".into()],
        process_limit: 64,
        directory: std::env::current_dir().unwrap(),
    });
    let (client, supervisor) = supervisor::channel::<Invocation<Capture>>(1).unwrap();
    let (stop, shutdown) = ShutdownSignal::new();
    let owner = tokio::spawn(supervisor.run(shutdown));
    for mode in ["echo", "fail", "stderr", "hang", "expired", "cancel", "echo"] {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut environment = Environment::new(1024);
        environment
            .insert(
                "CGI_MODE",
                if mode == "cancel" {
                    b"hang"
                } else {
                    mode.as_bytes()
                },
            )
            .unwrap();
        let body = vec![b'x'; 100_000];
        let request = Request {
            environment,
            input: Box::new(io::Cursor::new(body.clone())),
            output: Capture(seen.clone()),
            length: body.len() as u64,
            head_request: false,
            deadline: Instant::now()
                + if mode == "expired" {
                    Duration::ZERO
                } else if mode == "hang" {
                    Duration::from_millis(300)
                } else {
                    Duration::from_secs(5)
                },
            limits: Limits {
                max_stderr: 4,
                ..Limits::default()
            },
        };
        let mut running = Box::pin(client.execute(Invocation::new(program.clone(), request)));
        if mode == "cancel" {
            timeout(Duration::from_secs(5), async {
                loop {
                    tokio::select! {
                        result = &mut running => panic!("cancel fixture completed early: {result:?}"),
                        () = tokio::time::sleep(Duration::from_millis(10)) => {
                            if seen.lock().unwrap().heads > 0 { break; }
                        }
                    }
                }
            })
            .await
            .unwrap();
            drop(running);
            assert!(!seen.lock().unwrap().finished);
            // A deliberately invalid executable is an admission probe: it
            // cannot spawn, but reaches validation only after cleanup frees
            // capacity. Do not assume that an arbitrary sleep proves cleanup.
            timeout(Duration::from_secs(3), async {
                loop {
                    let probe = Invocation::new(
                        Arc::new(Program {
                            executable: "relative.exe".into(),
                            arguments: Vec::new(),
                            process_limit: 64,
                            directory: program.directory.clone(),
                        }),
                        Request {
                            environment: Environment::new(1),
                            input: Box::new(tokio::io::empty()),
                            output: Capture(Arc::new(Mutex::new(Seen::default()))),
                            length: 0,
                            head_request: false,
                            deadline: Instant::now() + Duration::from_secs(1),
                            limits: Limits::default(),
                        },
                    );
                    match client.execute(probe).await {
                        Err(supervisor::Error::Busy) => tokio::task::yield_now().await,
                        Err(supervisor::Error::Exchange(Error::Io(_))) => break,
                        result => panic!("unexpected cleanup probe: {result:?}"),
                    }
                }
            })
            .await
            .unwrap();
            continue;
        }
        let result = timeout(Duration::from_secs(7), running).await.unwrap();
        if mode == "echo" {
            assert_eq!(result.unwrap(), 0);
            let seen = seen.lock().unwrap();
            assert_eq!(seen.body, body);
            assert!(seen.finished);
        } else {
            assert!(!seen.lock().unwrap().finished);
            assert!(
                matches!(
                    result,
                    Err(supervisor::Error::Exchange(
                        Error::ExitFailure | Error::StderrLimit | Error::Timeout
                    ))
                ),
                "{mode}: {result:?}"
            );
        }
    }
    stop.request();
    timeout(Duration::from_secs(3), owner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
