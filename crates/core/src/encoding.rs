//! Encoding and decoding utilities.

const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decode a percent-encoded URI string.
pub fn percent_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_val(bytes[index + 1]), hex_val(bytes[index + 2]))
        {
            result.push(high << 4 | low);
            index += 3;
            continue;
        }
        result.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(result).unwrap_or_else(|_| input.to_string())
}

/// Decode a standard Base64 string.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let input = input.trim();
    if input.is_empty() {
        return Some(Vec::new());
    }

    let mut seen_padding = false;
    let mut padding = 0usize;
    let mut data_len = 0usize;
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for &byte in input.as_bytes() {
        if byte == b'=' {
            seen_padding = true;
            padding += 1;
            if padding > 2 {
                return None;
            }
            continue;
        }
        if seen_padding {
            return None;
        }
        let value = BASE64_TABLE
            .iter()
            .position(|&candidate| candidate == byte)? as u32;
        data_len += 1;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }

    let total_len = data_len + padding;
    if total_len % 4 == 1 || (padding > 0 && !total_len.is_multiple_of(4)) {
        return None;
    }

    Some(output)
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_round_trip() {
        assert_eq!(percent_decode("%2F"), "/");
    }

    #[test]
    fn base64_decoding() {
        assert_eq!(base64_decode("SGVsbG8="), Some(b"Hello".to_vec()));
    }

    #[test]
    fn base64_rejects_trailing_data_after_padding() {
        assert_eq!(base64_decode("SGVsbG8=bad"), None);
    }

    #[test]
    fn base64_rejects_invalid_padding_length() {
        assert_eq!(base64_decode("A="), None);
        assert_eq!(base64_decode("A==="), None);
    }
}
