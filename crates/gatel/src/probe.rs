//! A bounded, local HTTP probe for scratch images and service supervisors.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 8192;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum Kind {
    Ready,
    Live,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Live => "live",
        }
    }
}

#[derive(Debug, Clone, clap::Args)]
pub(crate) struct Options {
    /// Local plaintext HTTP listener containing the configured health route.
    #[arg(long, default_value = "127.0.0.1:8080", value_parser = address)]
    address: SocketAddr,
    /// Origin-form path of an explicitly configured health terminal.
    #[arg(long, default_value = "/readyz", value_parser = path)]
    path: String,
    /// Host header used to select the site; never used as a network target.
    #[arg(long, default_value = "localhost", value_parser = host)]
    host: String,
    #[arg(long, value_enum, default_value = "ready")]
    kind: Kind,
    /// One deadline for connect, request write and response headers.
    #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u64).range(1..=60000))]
    timeout_ms: u64,
}

fn address(input: &str) -> Result<SocketAddr, &'static str> {
    let address: SocketAddr = input.parse().map_err(|_| "expected a loopback IP:port")?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err("probe address must be loopback with a nonzero port");
    }
    Ok(address)
}

fn path(input: &str) -> Result<String, &'static str> {
    if input.len() > 4096
        || !input.starts_with('/')
        || input.starts_with("//")
        || input.contains('#')
        || !input.bytes().all(|b| (b'!'..=b'~').contains(&b))
    {
        return Err("probe path must be an ASCII origin-form path without whitespace or fragment");
    }
    Ok(input.to_string())
}

fn host(input: &str) -> Result<String, &'static str> {
    if input.is_empty()
        || input.len() > 512
        || !input
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:[]".contains(&b))
    {
        return Err("probe host must be an ASCII hostname or address");
    }
    Ok(input.to_string())
}

pub(crate) fn run(options: Options) -> ExitCode {
    match check(&options) {
        Ok(()) => match writeln!(std::io::stdout(), "{}", options.kind.as_str()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(super::RUNTIME_FAILURE),
        },
        Err(error) => {
            eprintln!("gatel: runtime probe failed: {error}");
            ExitCode::from(super::RUNTIME_FAILURE)
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, &'static str> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|value| !value.is_zero())
        .ok_or("deadline elapsed")
}

fn check(options: &Options) -> Result<(), &'static str> {
    let timeout = Duration::from_millis(options.timeout_ms);
    let deadline = Instant::now() + timeout;
    let mut stream =
        TcpStream::connect_timeout(&options.address, timeout).map_err(|_| "connection unavailable")?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCache-Control: no-cache\r\n\r\n",
        options.path, options.host
    );
    let mut pending = request.as_bytes();
    while !pending.is_empty() {
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|_| "cannot set write deadline")?;
        let count = stream.write(pending).map_err(|_| "request write failed")?;
        if count == 0 {
            return Err("request write stopped");
        }
        pending = &pending[count..];
    }
    let mut head = Vec::with_capacity(512);
    let mut buffer = [0; 512];
    loop {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|_| "cannot set read deadline")?;
        let count = stream
            .read(&mut buffer)
            .map_err(|_| "response unavailable before deadline")?;
        if count == 0 {
            return Err("response ended before headers");
        }
        head.extend_from_slice(&buffer[..count]);
        if let Some(end) = head.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > MAX_HEAD {
                return Err("response headers too large");
            }
            return accepted(&head[..end], options.kind);
        }
        if head.len() >= MAX_HEAD {
            return Err("response headers too large");
        }
    }
}

fn accepted(head: &[u8], kind: Kind) -> Result<(), &'static str> {
    let head = std::str::from_utf8(head).map_err(|_| "invalid response headers")?;
    let mut lines = head.split("\r\n");
    let mut status = lines.next().ok_or("missing response status")?.split_whitespace();
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) || status.next() != Some("200") {
        return Err("runtime is not healthy");
    }
    let mut marker = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("invalid response header")?;
        if name.eq_ignore_ascii_case("x-gatel-health") {
            if marker.is_some() {
                return Err("ambiguous health response");
            }
            marker = Some(value.trim());
        }
    }
    if marker != Some(kind.as_str()) {
        return Err("response is not the requested Gatel health terminal");
    }
    Ok(())
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
