//! Incremental CGI response header parser. Stops exactly before body bytes.
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};

use crate::Error;

#[derive(Debug)]
pub struct Head {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub content_length: Option<u64>,
}

/// Streaming response accounting. Validate each chunk before forwarding it;
/// reaching EOF does not make a truncated Content-Length response successful.
pub struct BodyLength {
    expected: Option<u64>,
    received: u64,
}

impl BodyLength {
    pub fn new(head: &Head, is_head_request: bool) -> Self {
        let no_body = is_head_request || matches!(head.status.as_u16(), 204 | 304);
        Self {
            expected: if no_body { Some(0) } else { head.content_length },
            received: 0,
        }
    }

    pub fn observe(&mut self, bytes: usize) -> Result<(), Error> {
        let total = self
            .received
            .checked_add(u64::try_from(bytes).map_err(|_| Error::BodyLength)?)
            .ok_or(Error::BodyLength)?;
        if self.expected.is_some_and(|expected| total > expected) {
            return Err(Error::BodyLength);
        }
        self.received = total;
        Ok(())
    }

    pub fn finish(&self) -> Result<(), Error> {
        if self.expected.is_some_and(|expected| expected != self.received) {
            Err(Error::BodyLength)
        } else {
            Ok(())
        }
    }
}

pub struct Decoder {
    bytes: Vec<u8>,
    limit: usize,
    finished: bool,
}

impl Decoder {
    pub const MAX_BYTES: usize = 64 * 1024;
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit: limit.min(Self::MAX_BYTES),
            finished: false,
        }
    }

    /// Returns bytes consumed and a completed head, if any. Body bytes stay in
    /// the caller's buffer; a large first body chunk cannot exceed the head cap.
    pub fn feed(&mut self, input: &[u8]) -> Result<(usize, Option<Head>), Error> {
        if self.finished {
            return Err(Error::StreamClosed);
        }
        for (index, byte) in input.iter().enumerate() {
            if self.bytes.len() == self.limit {
                return Err(Error::Limit);
            }
            self.bytes.push(*byte);
            if self.bytes.ends_with(b"\r\n\r\n") || self.bytes.ends_with(b"\n\n") {
                let head = parse(&self.bytes)?;
                self.finished = true;
                return Ok((index + 1, Some(head)));
            }
        }
        Ok((input.len(), None))
    }

    pub fn eof(&self) -> Result<(), Error> {
        if self.finished {
            Ok(())
        } else {
            Err(Error::IncompleteResponse)
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Head, Error> {
    let mut headers = HeaderMap::new();
    let mut status = None;
    let mut content_length = None;
    for raw in bytes.split(|b| *b == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            break;
        }
        if line.iter().any(|byte| byte.is_ascii_control() && *byte != b'\t') {
            return Err(Error::InvalidHeader);
        }
        if line[0].is_ascii_whitespace() {
            return Err(Error::InvalidHeader);
        }
        let colon = line.iter().position(|b| *b == b':').ok_or(Error::InvalidHeader)?;
        let name = HeaderName::from_bytes(&line[..colon]).map_err(|_| Error::InvalidHeader)?;
        let value = line[colon + 1..].trim_ascii();
        // Do not let CGI control HTTP framing or connection ownership.
        if matches!(
            name.as_str(),
            "connection"
                | "transfer-encoding"
                | "keep-alive"
                | "upgrade"
                | "trailer"
                | "te"
                | "proxy-connection"
                | "proxy-authenticate"
                | "proxy-authorization"
        ) {
            return Err(Error::InvalidHeader);
        }
        if name == "status" {
            if status.is_some() {
                return Err(Error::DuplicateHeader);
            }
            if value.len() < 3
                || !value[..3].iter().all(u8::is_ascii_digit)
                || (value.len() > 3 && value[3] != b' ')
                || HeaderValue::from_bytes(value).is_err()
            {
                return Err(Error::InvalidStatus);
            }
            let code = std::str::from_utf8(&value[..3]).unwrap().parse::<u16>().unwrap();
            if !(200..=599).contains(&code) {
                return Err(Error::InvalidStatus);
            }
            status = Some(StatusCode::from_u16(code).map_err(|_| Error::InvalidStatus)?);
            continue;
        }
        if matches!(name.as_str(), "content-length" | "content-type" | "location")
            && headers.contains_key(&name)
        {
            return Err(Error::DuplicateHeader);
        }
        if name == "content-length" {
            if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return Err(Error::InvalidHeader);
            }
            content_length = Some(
                std::str::from_utf8(value)
                    .unwrap()
                    .parse::<u64>()
                    .map_err(|_| Error::InvalidHeader)?,
            );
        }
        headers.append(
            name,
            HeaderValue::from_bytes(value).map_err(|_| Error::InvalidHeader)?,
        );
    }
    if status.is_none()
        && let Some(location) = headers.get("location")
    {
        if location.as_bytes().starts_with(b"/") {
            return Err(Error::UnsupportedLocalRedirect);
        }
        let uri = location
            .to_str()
            .ok()
            .and_then(|s| s.parse::<http::Uri>().ok())
            .ok_or(Error::InvalidHeader)?;
        if uri.scheme().is_none() {
            return Err(Error::InvalidHeader);
        }
        status = Some(StatusCode::FOUND);
    }
    let status = status.unwrap_or(StatusCode::OK);
    if status == StatusCode::NO_CONTENT && content_length.is_some() {
        return Err(Error::InvalidHeader);
    }
    if !headers.contains_key("content-type")
        && !headers.contains_key("location")
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED
    {
        return Err(Error::InvalidHeader);
    }
    Ok(Head {
        status,
        headers,
        content_length,
    })
}
