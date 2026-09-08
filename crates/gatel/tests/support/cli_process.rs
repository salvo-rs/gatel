//! Included by the CLI integration test; no pipe that can fill while waiting.
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("gatel-cli-capture-{}", gatel_model::BootId::generate()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn output(command: &mut Command) -> Output {
    output_with_timeout(command, Duration::from_secs(120)).unwrap_or_else(|timeout| {
        panic!(
            "CLI fixture pid={} exceeded deadline after {:?}; status={}; stdout={} stderr={}",
            timeout.pid,
            timeout.elapsed,
            timeout.output.status,
            String::from_utf8_lossy(&timeout.output.stdout),
            String::from_utf8_lossy(&timeout.output.stderr)
        );
    })
}

#[derive(Debug)]
pub(super) struct TimedOut {
    pub pid: u32,
    pub elapsed: Duration,
    pub output: Output,
}

pub(super) fn output_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output, TimedOut> {
    let directory = Directory::new();
    let stdout = directory.0.join("stdout");
    let stderr = directory.0.join("stderr");
    command
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap());
    let started = Instant::now();
    let mut process = Process(command.spawn().unwrap());
    eprintln!(
        "CLI fixture pid={} started after {:?}",
        process.0.id(),
        started.elapsed()
    );
    let read = |path| {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .unwrap()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(
            bytes.len() <= 1024 * 1024,
            "CLI fixture output exceeded 1 MiB capture limit"
        );
        bytes
    };
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            eprintln!(
                "CLI fixture pid={} elapsed={:?}",
                process.0.id(),
                started.elapsed()
            );
            return Ok(Output {
                status,
                stdout: read(&stdout),
                stderr: read(&stderr),
            });
        }
        if started.elapsed() >= timeout {
            // It may have exited between try_wait and kill. Reap either way;
            // failure to kill a still-running child must not look like success.
            if let Err(error) = process.0.kill() {
                assert!(
                    process.0.try_wait().unwrap().is_some(),
                    "cannot stop fixture: {error}"
                );
            }
            let status = process.0.wait().unwrap();
            return Err(TimedOut {
                pid: process.0.id(),
                elapsed: started.elapsed(),
                output: Output {
                    status,
                    stdout: read(&stdout),
                    stderr: read(&stderr),
                },
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_capture_is_not_silently_truncated() {
        const FIXTURE: &str = "GATEL_CLI_OVERSIZED_OUTPUT_FIXTURE";
        if std::env::var(FIXTURE).as_deref() == Ok("child") {
            use std::io::Write;
            std::io::stdout().write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap();
            std::io::stdout().flush().unwrap();
            return;
        }
        let rejected = std::panic::catch_unwind(|| {
            output_with_timeout(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "process_output::tests::oversized_capture_is_not_silently_truncated",
                        "--nocapture",
                    ])
                    .env(FIXTURE, "child"),
                Duration::from_secs(10),
            )
        });
        let error = rejected.expect_err("oversized output must not become an ordinary truncated Output");
        let message = error
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| error.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("output exceeded 1 MiB capture limit"),
            "unexpected panic: {message}"
        );
    }
}
