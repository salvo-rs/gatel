//! Owned stdio exchange; stdout EOF and successful process exit are independent.
use std::future::Future;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use gatel_cgi_protocol::head::{BodyLength, Decoder, Head};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Protocol(gatel_cgi_protocol::Error),
    Timeout,
    RequestLength,
    StderrLimit,
    ExitFailure,
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<gatel_cgi_protocol::Error> for Error {
    fn from(error: gatel_cgi_protocol::Error) -> Self {
        Self::Protocol(error)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "CGI I/O failure ({:?})", error.kind()),
            Self::Protocol(error) => write!(f, "{error}"),
            Self::Timeout => f.write_str("CGI exchange deadline exceeded"),
            Self::RequestLength => f.write_str("CGI request length mismatch or limit exceeded"),
            Self::StderrLimit => f.write_str("CGI stderr budget exceeded"),
            Self::ExitFailure => f.write_str("CGI process did not exit successfully"),
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
    pub max_stderr: u64,
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
pub trait Output: Send {
    fn head(&mut self, head: Head) -> impl Future<Output = io::Result<()>> + Send;
    fn body(&mut self, bytes: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn finish(&mut self) -> impl Future<Output = io::Result<()>> + Send;
}

/// All pipe handles and input are owned by this invocation. Cancelling drops
/// them, but the caller's supervisor MUST still kill/reap the process tree and
/// hold its process permit until cleanup completes. No detached tasks here.
/// The exit future must represent a supervised process's verified exit status.
#[allow(clippy::too_many_arguments)]
pub async fn run<W, R, E, I, F, O>(
    stdin: W,
    stdout: R,
    stderr: E,
    input: I,
    exit: F,
    request_length: u64,
    head_request: bool,
    output: &mut O,
    limits: Limits,
) -> Result<u64, Error>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
    E: AsyncRead + Unpin + Send,
    I: AsyncRead + Unpin + Send,
    F: Future<Output = io::Result<ExitStatus>> + Send,
    O: Output,
{
    if request_length > limits.max_request {
        return Err(Error::RequestLength);
    }
    let operation = async {
        let transfer = async {
            let sending = send(stdin, input, request_length, limits.idle);
            let receiving = receive(stdout, output, head_request, limits);
            tokio::pin!(sending, receiving);
            tokio::select! {
                biased;
                result = &mut receiving => result,
                result = &mut sending => { result?; receiving.await },
            }
        };
        let status = async {
            if exit.await?.success() {
                Ok(())
            } else {
                Err(Error::ExitFailure)
            }
        };
        let (tail, stderr_bytes, ()) = tokio::try_join!(transfer, drain(stderr, limits.max_stderr), status)?;
        if let Some(byte) = tail {
            bounded(limits.idle, output.body(&[byte])).await?;
        }
        bounded(limits.idle, output.finish()).await?;
        Ok(stderr_bytes)
    };
    tokio::time::timeout(limits.overall, operation)
        .await
        .map_err(|_| Error::Timeout)?
}

async fn bounded<T>(duration: Duration, work: impl Future<Output = io::Result<T>>) -> Result<T, Error> {
    tokio::time::timeout(duration, work)
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(Error::Io)
}
async fn send<W: AsyncWrite + Unpin, I: AsyncRead + Unpin>(
    mut writer: W,
    mut input: I,
    mut remaining: u64,
    idle: Duration,
) -> Result<(), Error> {
    let mut bytes = [0; 8192];
    while remaining > 0 {
        let size = remaining.min(bytes.len() as u64) as usize;
        let count = bounded(idle, input.read(&mut bytes[..size])).await?;
        if count == 0 {
            return Err(Error::RequestLength);
        }
        bounded(idle, writer.write_all(&bytes[..count])).await?;
        remaining -= count as u64;
    }
    if bounded(idle, input.read(&mut bytes[..1])).await? != 0 {
        return Err(Error::RequestLength);
    }
    bounded(idle, writer.shutdown()).await?;
    Ok(())
}
async fn drain<E: AsyncRead + Unpin>(mut stderr: E, limit: u64) -> Result<u64, Error> {
    let mut total = 0_u64;
    let mut bytes = [0; 8192];
    loop {
        // Silence on stderr is normal: only the invocation's overall deadline
        // bounds this read. Never retain or log stderr payloads.
        let size = limit
            .saturating_sub(total)
            .saturating_add(1)
            .min(bytes.len() as u64) as usize;
        let count = stderr.read(&mut bytes[..size]).await?;
        if count == 0 {
            return Ok(total);
        }
        total = total
            .checked_add(count as u64)
            .filter(|total| *total <= limit)
            .ok_or(Error::StderrLimit)?;
    }
}
async fn receive<R: AsyncRead + Unpin, O: Output>(
    mut reader: R,
    output: &mut O,
    head_request: bool,
    limits: Limits,
) -> Result<Option<u8>, Error> {
    let mut decoder = Decoder::new(limits.max_head);
    let mut length = None;
    let mut tail = None;
    let mut first = true;
    let mut bytes = [0; 8192];
    loop {
        let count = bounded(
            if first { limits.overall } else { limits.idle },
            reader.read(&mut bytes),
        )
        .await?;
        first = false;
        if count == 0 {
            decoder.eof()?;
            length
                .as_ref()
                .ok_or(gatel_cgi_protocol::Error::IncompleteResponse)
                .and_then(BodyLength::finish)?;
            // The last byte is not published until stderr EOF AND exit succeed.
            return Ok(tail);
        }
        let mut body = &bytes[..count];
        if length.is_none() {
            let (used, head) = decoder.feed(body)?;
            body = &body[used..];
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
}
