//! Concurrent stdin upload and stdout/stderr receive over one owned transport.
use std::future::Future;
use std::io;
use std::time::Duration;

use gatel_cgi_protocol::environment::Environment;
use gatel_cgi_protocol::fastcgi as fcgi;
use gatel_cgi_protocol::head::{BodyLength, Decoder, Head};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Protocol(gatel_cgi_protocol::Error),
    Timeout,
    RequestLength,
    ApplicationExit(u32),
}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<gatel_cgi_protocol::Error> for Error {
    fn from(value: gatel_cgi_protocol::Error) -> Self {
        Self::Protocol(value)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Backend text and stderr are intentionally not included.
        match self {
            Self::Io(error) => write!(f, "FastCGI I/O failure ({:?})", error.kind()),
            Self::Protocol(error) => write!(f, "{error}"),
            Self::Timeout => f.write_str("FastCGI exchange deadline exceeded"),
            Self::RequestLength => f.write_str("FastCGI request length mismatch or limit exceeded"),
            Self::ApplicationExit(code) => write!(f, "FastCGI application exited with code {code}"),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub overall: Duration,
    pub idle: Duration,
    pub max_request: u64,
    pub max_head: usize,
    pub max_stderr: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            overall: Duration::from_secs(60),
            idle: Duration::from_secs(15),
            max_request: 16 * 1024 * 1024,
            max_head: 64 * 1024,
            max_stderr: 64 * 1024,
        }
    }
}

/// Backpressure-aware output. Never mark a response complete before `finish`:
/// a late protocol error must terminate the stream, not appear as clean EOF.
pub trait Output: Send {
    fn head(&mut self, head: Head) -> impl Future<Output = io::Result<()>> + Send;
    fn body(&mut self, bytes: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn finish(&mut self) -> impl Future<Output = io::Result<()>> + Send;
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

/// Transport is consumed and closed on completion, timeout or future drop.
/// Each invocation is one responder request; no KEEP_CONN or multiplexing.
pub async fn run<S, R, O>(
    stream: S,
    input: R,
    environment: &Environment,
    request_length: u64,
    head_request: bool,
    output: &mut O,
    limits: Limits,
) -> Result<Report, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send,
    O: Output,
{
    if request_length > limits.max_request {
        return Err(Error::RequestLength);
    }
    let length = environment
        .iter()
        .find(|(name, _)| *name == "CONTENT_LENGTH")
        .filter(|(_, value)| !value.is_empty() && value.iter().all(u8::is_ascii_digit))
        .and_then(|(_, value)| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u64>().ok());
    if length != Some(request_length) {
        return Err(Error::RequestLength);
    }
    let parameters = fcgi::params(environment)?;
    let (reader, writer) = tokio::io::split(stream);
    let operation = async {
        let sending = send(writer, input, parameters, request_length, limits.idle);
        let receiving = receive(reader, output, head_request, limits);
        tokio::pin!(sending, receiving);
        tokio::select! {
            // A complete early response cancels a stalled upload. No detached
            // writer remains holding the socket or waiting on the client body.
            biased;
            result = &mut receiving => result,
            result = &mut sending => {
                result?;
                receiving.await
            }
        }
    };
    tokio::time::timeout(limits.overall, operation)
        .await
        .map_err(|_| Error::Timeout)?
}

async fn bounded<T>(idle: Duration, future: impl Future<Output = io::Result<T>>) -> Result<T, Error> {
    tokio::time::timeout(idle, future)
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(Error::Io)
}

async fn send<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    mut writer: W,
    mut input: R,
    parameters: Vec<u8>,
    length: u64,
    idle: Duration,
) -> Result<(), Error> {
    bounded(idle, writer.write_all(&fcgi::begin())).await?;
    bounded(idle, writer.write_all(&parameters)).await?;
    let mut remaining = length;
    let mut chunk = [0; 8192];
    while remaining > 0 {
        let want = remaining.min(chunk.len() as u64) as usize;
        let read = bounded(idle, input.read(&mut chunk[..want])).await?;
        if read == 0 {
            return Err(Error::RequestLength);
        }
        bounded(
            idle,
            writer.write_all(&fcgi::record(fcgi::STDIN, &chunk[..read])?),
        )
        .await?;
        remaining -= read as u64;
    }
    // Input is required to represent exactly one framed HTTP body.
    if bounded(idle, input.read(&mut chunk[..1])).await? != 0 {
        return Err(Error::RequestLength);
    }
    bounded(idle, writer.write_all(&fcgi::record(fcgi::STDIN, &[])?)).await?;
    bounded(idle, writer.flush()).await?;
    Ok(())
}

async fn receive<R: AsyncRead + Unpin, O: Output>(
    mut reader: R,
    output: &mut O,
    head_request: bool,
    limits: Limits,
) -> Result<Report, Error> {
    let mut responder = fcgi::Responder::new(limits.max_stderr);
    let mut decoder = Decoder::new(limits.max_head);
    let mut length = None;
    let mut report = Report::default();
    // Never let HTTP satisfy Content-Length before END_REQUEST is validated.
    // One byte is enough; retaining an entire response would destroy streaming.
    let mut tail = None;
    let mut wire = vec![0; 8 + u16::MAX as usize + u8::MAX as usize];
    let mut first_record = true;
    loop {
        // A backend may wait for all stdin before writing its first record.
        // Upload I/O is bounded separately; do not mistake a progressing slow
        // upload for backend idleness. The overall deadline still applies.
        let wait = if first_record { limits.overall } else { limits.idle };
        bounded(wait, reader.read_exact(&mut wire[..8])).await?;
        first_record = false;
        // Validate the prefix before reading an advertised payload.
        fcgi::decode(&wire[..8])?;
        let content = u16::from_be_bytes([wire[4], wire[5]]) as usize;
        let total = 8 + content + wire[6] as usize;
        bounded(limits.idle, reader.read_exact(&mut wire[8..total])).await?;
        let (record, _) = fcgi::decode(&wire[..total])?.ok_or(gatel_cgi_protocol::Error::InvalidRecord)?;
        match responder.accept(record)? {
            fcgi::Event::Stdout(bytes) => {
                report.stdout_bytes += bytes.len() as u64;
                let mut body = bytes;
                if length.is_none() {
                    let (used, head) = decoder.feed(bytes)?;
                    body = &bytes[used..];
                    if let Some(head) = head {
                        length = Some(BodyLength::new(&head, head_request));
                        bounded(limits.idle, output.head(head)).await?;
                    }
                }
                if !body.is_empty() {
                    length
                        .as_mut()
                        .ok_or(gatel_cgi_protocol::Error::IncompleteResponse)?
                        .observe(body.len())?;
                    if let Some(byte) = tail.take() {
                        bounded(limits.idle, output.body(&[byte])).await?;
                    }
                    if body.len() > 1 {
                        bounded(limits.idle, output.body(&body[..body.len() - 1])).await?;
                    }
                    tail = body.last().copied();
                }
            }
            fcgi::Event::Stderr(bytes) => {
                report.stderr_bytes += bytes.len() as u64;
            }
            fcgi::Event::Complete { app_status } => {
                decoder.eof()?;
                length
                    .as_ref()
                    .ok_or(gatel_cgi_protocol::Error::IncompleteResponse)?
                    .finish()?;
                if app_status != 0 {
                    return Err(Error::ApplicationExit(app_status));
                }
                if let Some(byte) = tail.take() {
                    bounded(limits.idle, output.body(&[byte])).await?;
                }
                bounded(limits.idle, output.finish()).await?;
                return Ok(report);
            }
        }
    }
}
