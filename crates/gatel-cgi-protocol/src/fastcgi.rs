//! FastCGI v1 responder framing; one request per transport, no multiplexing.
use crate::Error;
use crate::environment::Environment;
pub const BEGIN: u8 = 1;
pub const END: u8 = 3;
pub const PARAMS: u8 = 4;
pub const STDIN: u8 = 5;
pub const STDOUT: u8 = 6;
pub const STDERR: u8 = 7;
pub const REQUEST: u16 = 1;

#[derive(Debug)]
pub struct Record<'a> {
    pub kind: u8,
    pub request: u16,
    pub body: &'a [u8],
}

/// A borrowed record and total consumed bytes (including padding). None means
/// incomplete input, never an instruction to allocate the advertised length.
pub fn decode(input: &[u8]) -> Result<Option<(Record<'_>, usize)>, Error> {
    if input.len() < 8 {
        return Ok(None);
    }
    if input[0] != 1 {
        return Err(Error::InvalidRecord);
    }
    let length = u16::from_be_bytes([input[4], input[5]]) as usize;
    let end = 8 + length + input[6] as usize;
    if input.len() < end {
        return Ok(None);
    }
    Ok(Some((
        Record {
            kind: input[1],
            request: u16::from_be_bytes([input[2], input[3]]),
            body: &input[8..8 + length],
        },
        end,
    )))
}

pub fn record(kind: u8, body: &[u8]) -> Result<Vec<u8>, Error> {
    let length = u16::try_from(body.len()).map_err(|_| Error::Limit)?.to_be_bytes();
    let mut output = vec![1, kind, 0, 1, length[0], length[1], 0, 0];
    output.extend_from_slice(body);
    Ok(output)
}

pub fn begin() -> Vec<u8> {
    // Responder role, no KEEP_CONN: cancellation closes this request's transport.
    record(BEGIN, &[0, 1, 0, 0, 0, 0, 0, 0]).unwrap()
}

pub fn params(environment: &Environment) -> Result<Vec<u8>, Error> {
    let mut stream = Vec::new();
    for (name, value) in environment.iter() {
        length(name.len(), &mut stream)?;
        length(value.len(), &mut stream)?;
        stream.extend_from_slice(name.as_bytes());
        stream.extend_from_slice(value);
    }
    let mut output = Vec::new();
    for chunk in stream.chunks(u16::MAX as usize) {
        output.extend(record(PARAMS, chunk)?);
    }
    output.extend(record(PARAMS, &[])?);
    Ok(output)
}

fn length(length: usize, output: &mut Vec<u8>) -> Result<(), Error> {
    let length = u32::try_from(length).map_err(|_| Error::Limit)?;
    if length > 0x7fff_ffff {
        return Err(Error::Limit);
    }
    if length < 128 {
        output.push(length as u8);
    } else {
        output.extend_from_slice(&(length | 0x8000_0000).to_be_bytes());
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    Stdout(&'a [u8]),
    Stderr(&'a [u8]),
    Complete { app_status: u32 },
}

pub struct Responder {
    stdout_closed: bool,
    stderr_closed: bool,
    ended: bool,
    stderr_bytes: usize,
    stderr_limit: usize,
}

impl Responder {
    pub fn new(stderr_limit: usize) -> Self {
        Self {
            stdout_closed: false,
            stderr_closed: false,
            ended: false,
            stderr_bytes: 0,
            stderr_limit,
        }
    }

    pub fn accept<'a>(&mut self, record: Record<'a>) -> Result<Event<'a>, Error> {
        if self.ended {
            return Err(Error::StreamClosed);
        }
        if record.request != REQUEST {
            return Err(Error::UnexpectedRecord);
        }
        match record.kind {
            STDOUT => {
                if self.stdout_closed {
                    return Err(Error::StreamClosed);
                }
                self.stdout_closed = record.body.is_empty();
                Ok(Event::Stdout(record.body))
            }
            STDERR => {
                if self.stderr_closed {
                    return Err(Error::StreamClosed);
                }
                self.stderr_bytes = self
                    .stderr_bytes
                    .checked_add(record.body.len())
                    .filter(|n| *n <= self.stderr_limit)
                    .ok_or(Error::Limit)?;
                self.stderr_closed = record.body.is_empty();
                Ok(Event::Stderr(record.body))
            }
            END => {
                if record.body.len() != 8 {
                    return Err(Error::InvalidRecord);
                }
                if record.body[4] != 0 {
                    return Err(Error::BackendRejected);
                }
                // PHP-FPM may close stdout implicitly with END_REQUEST. The
                // adapter still validates response headers and body length;
                // transport EOF alone is never a successful terminator.
                self.stdout_closed = true;
                self.stderr_closed = true;
                self.ended = true;
                Ok(Event::Complete {
                    app_status: u32::from_be_bytes(record.body[..4].try_into().unwrap()),
                })
            }
            _ => Err(Error::UnexpectedRecord),
        }
    }

    pub fn eof(&self) -> Result<(), Error> {
        if self.ended {
            Ok(())
        } else {
            Err(Error::IncompleteResponse)
        }
    }
}
