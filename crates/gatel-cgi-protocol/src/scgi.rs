//! SCGI request netstring. Response framing belongs to the transport profile:
//! unlike FastCGI, SCGI has no records, END_REQUEST or stderr channel.
use crate::Error;
use crate::environment::Environment;

/// Encode only the request headers; the caller streams exactly `body_length`
/// bytes immediately afterwards. CONTENT_LENGTH must already agree with the
/// input contract. SCGI=1 is supplied here, never inherited from the process.
/// `max_headers` bounds the netstring payload, excluding decimal framing.
pub fn headers(environment: &Environment, body_length: u64, max_headers: usize) -> Result<Vec<u8>, Error> {
    let mut content_length = None;
    let mut size = 7usize; // SCGI NUL 1 NUL
    for (name, value) in environment.iter() {
        if name == "SCGI" {
            if value != b"1" {
                return Err(Error::InvalidEnvironment);
            }
            continue;
        }
        if name == "CONTENT_LENGTH" {
            if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return Err(Error::InvalidEnvironment);
            }
            let length = value
                .iter()
                .try_fold(0u64, |n, byte| {
                    n.checked_mul(10)?.checked_add((byte - b'0') as u64)
                })
                .ok_or(Error::BodyLength)?;
            if length != body_length {
                return Err(Error::BodyLength);
            }
            content_length = Some(value);
        }
        size = size
            .checked_add(name.len())
            .and_then(|n| n.checked_add(value.len()))
            .and_then(|n| n.checked_add(2))
            .ok_or(Error::Limit)?;
    }
    let content_length = content_length.ok_or(Error::InvalidEnvironment)?;
    if size > max_headers.min(Environment::MAX_BYTES) {
        return Err(Error::Limit);
    }
    let prefix = size.to_string();
    let mut output = Vec::with_capacity(size + prefix.len() + 2);
    output.extend_from_slice(prefix.as_bytes());
    output.push(b':');
    pair(&mut output, "CONTENT_LENGTH", content_length);
    pair(&mut output, "SCGI", b"1");
    for (name, value) in environment.iter() {
        if name != "CONTENT_LENGTH" && name != "SCGI" {
            pair(&mut output, name, value);
        }
    }
    output.push(b',');
    Ok(output)
}

fn pair(output: &mut Vec<u8>, name: &str, value: &[u8]) {
    output.extend_from_slice(name.as_bytes());
    output.push(0);
    output.extend_from_slice(value);
    output.push(0);
}
