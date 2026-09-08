//! One owned SCGI connection, concurrent upload/download, CGI response profile.
use std::future::Future;
use std::io;
use std::time::Duration;

use gatel_cgi_protocol::environment::Environment;
use gatel_cgi_protocol::head::{BodyLength, Decoder, Head};
use gatel_cgi_protocol::scgi;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Protocol(gatel_cgi_protocol::Error),
    Timeout,
    RequestLength,
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
        match self {
            Self::Io(error) => write!(f, "SCGI I/O failure ({:?})", error.kind()),
            Self::Protocol(error) => write!(f, "{error}"),
            Self::Timeout => f.write_str("SCGI exchange deadline exceeded"),
            Self::RequestLength => f.write_str("SCGI request length mismatch or limit exceeded"),
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
    pub max_environment: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            overall: Duration::from_secs(60),
            idle: Duration::from_secs(15),
            max_request: 16 * 1024 * 1024,
            max_head: 64 * 1024,
            max_environment: 64 * 1024,
        }
    }
}

/// All callbacks apply backpressure. An error after head must abort the HTTP
/// stream; only finish signals validated completion.
pub trait Output: Send {
    fn head(&mut self, head: Head) -> impl Future<Output = io::Result<()>> + Send;
    fn body(&mut self, bytes: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn finish(&mut self) -> impl Future<Output = io::Result<()>> + Send;
}

/// Closing/abandoning this future drops both socket halves and input. An early
/// complete backend response cancels upload. No reuse, detached task or stderr.
pub async fn run<S, R, O>(
    stream: S,
    input: R,
    env: &Environment,
    request_length: u64,
    head_request: bool,
    output: &mut O,
    limits: Limits,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send,
    O: Output,
{
    if request_length > limits.max_request {
        return Err(Error::RequestLength);
    }
    let headers = scgi::headers(env, request_length, limits.max_environment)?;
    let (reader, writer) = tokio::io::split(stream);
    let operation = async {
        let sending = send(writer, input, headers, request_length, limits.idle);
        let receiving = receive(reader, output, head_request, limits);
        tokio::pin!(sending, receiving);
        tokio::select! {
            biased;
            result = &mut receiving => result,
            result = &mut sending => { result?; receiving.await },
        }
    };
    tokio::time::timeout(limits.overall, operation)
        .await
        .map_err(|_| Error::Timeout)?
}

async fn bounded<T>(duration: Duration, future: impl Future<Output = io::Result<T>>) -> Result<T, Error> {
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(Error::Io)
}

async fn send<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    mut writer: W,
    mut input: R,
    headers: Vec<u8>,
    length: u64,
    idle: Duration,
) -> Result<(), Error> {
    bounded(idle, writer.write_all(&headers)).await?;
    let mut remaining = length;
    let mut bytes = [0; 8192];
    while remaining > 0 {
        let want = remaining.min(bytes.len() as u64) as usize;
        let count = bounded(idle, input.read(&mut bytes[..want])).await?;
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

async fn receive<R: AsyncRead + Unpin, O: Output>(
    mut reader: R,
    output: &mut O,
    head_request: bool,
    limits: Limits,
) -> Result<(), Error> {
    let mut decoder = Decoder::new(limits.max_head);
    let mut length = None;
    let mut tail = None;
    let mut first = true;
    let mut bytes = [0; 8192];
    loop {
        // A progressing upload may precede all response bytes.
        let wait = if first { limits.overall } else { limits.idle };
        let count = bounded(wait, reader.read(&mut bytes)).await?;
        first = false;
        if count == 0 {
            decoder.eof()?;
            length
                .as_ref()
                .ok_or(gatel_cgi_protocol::Error::IncompleteResponse)
                .and_then(BodyLength::finish)?;
            if let Some(byte) = tail {
                bounded(limits.idle, output.body(&[byte])).await?;
            }
            bounded(limits.idle, output.finish()).await?;
            return Ok(());
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
            // Do not satisfy HTTP Content-Length until EOF proves no overrun.
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
