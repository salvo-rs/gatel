//! PROXY protocol v1/v2 parser for extracting the real client address from
//! load balancers, CDNs, or other proxies that prepend the PROXY protocol
//! header to TCP connections.
//!
//! # Protocol overview
//!
//! - **v1** (text): `PROXY TCP4 192.168.1.1 192.168.1.2 12345 80\r\n`
//! - **v2** (binary): 12-byte signature + version/command + family/transport + length + addresses
//!
//! This module also provides [`PrefixedStream`], a wrapper around `TcpStream` that
//! prepends buffered bytes (the leftover data after the PROXY header) so the
//! rest of the connection can be read normally.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::ProxyError;

/// The 12-byte signature that identifies a PROXY protocol v2 header.
const V2_SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";

/// The maximum length of a PROXY protocol v1 line (per spec: 107 bytes including CRLF).
const V1_MAX_LINE_LEN: usize = 108;

/// Maximum header size to read before giving up (v2 can have TLVs, but we cap it).
const MAX_HEADER_SIZE: usize = 536;

/// Parsed result of a PROXY protocol header.
#[derive(Debug, Clone)]
pub struct ProxyProtocolHeader {
    /// The real source (client) address.
    pub src_addr: SocketAddr,
    /// The destination address (may be None for LOCAL commands in v2).
    pub dst_addr: Option<SocketAddr>,
}

/// Parse the PROXY protocol header from the beginning of a TCP stream.
///
/// Returns the parsed header and a [`PrefixedStream`] that wraps the original
/// stream with any leftover bytes prepended so subsequent reads see the actual
/// application data.
///
/// If the stream does not start with a PROXY protocol header, returns `None`
/// and the stream is returned as-is inside a `PrefixedStream` with the peeked
/// bytes prepended.
pub async fn parse_proxy_protocol(
    stream: &mut TcpStream,
) -> Result<(Option<ProxyProtocolHeader>, Vec<u8>), ProxyError> {
    // Read enough bytes to determine the protocol version.
    // We need at least 16 bytes to check for v2 (12 signature + 4 header).
    // For v1, we need "PROXY " (6 bytes) as a prefix.
    let mut buf = vec![0u8; MAX_HEADER_SIZE];
    let mut total_read = 0;

    // Read the initial bytes. We need at least 16 for v2 detection.
    while total_read < 16 {
        let n = stream.read(&mut buf[total_read..]).await?;
        if n == 0 {
            return Err(ProxyError::Internal(
                "connection closed before PROXY protocol header".into(),
            ));
        }
        total_read += n;
    }

    // Check for v2 signature.
    if buf[..12] == *V2_SIGNATURE {
        return parse_v2(&mut buf, total_read, stream).await;
    }

    // Check for v1 prefix.
    if buf.starts_with(b"PROXY ") {
        return parse_v1(&mut buf, total_read, stream).await;
    }

    // Not a PROXY protocol header — return all buffered bytes as prefix.
    debug!("no PROXY protocol header detected, passing through");
    let prefix = buf[..total_read].to_vec();
    Ok((None, prefix))
}

/// Parse a PROXY protocol v1 (text) header.
#[allow(clippy::ptr_arg)]
async fn parse_v1(
    buf: &mut Vec<u8>,
    mut total_read: usize,
    stream: &mut TcpStream,
) -> Result<(Option<ProxyProtocolHeader>, Vec<u8>), ProxyError> {
    // Read until we find \r\n or hit the max line length.
    loop {
        if let Some(pos) = buf[..total_read].windows(2).position(|w| w == b"\r\n") {
            let line = std::str::from_utf8(&buf[..pos])
                .map_err(|_| ProxyError::Internal("PROXY v1 header is not valid UTF-8".into()))?;

            let header = parse_v1_line(line)?;
            let remaining = buf[pos + 2..total_read].to_vec();

            debug!(
                src = %header.src_addr,
                dst = ?header.dst_addr,
                "parsed PROXY protocol v1 header"
            );

            return Ok((Some(header), remaining));
        }

        if total_read >= V1_MAX_LINE_LEN {
            return Err(ProxyError::Internal(
                "PROXY v1 header too long (no CRLF found)".into(),
            ));
        }

        // Read more data.
        let n = stream.read(&mut buf[total_read..]).await?;
        if n == 0 {
            return Err(ProxyError::Internal(
                "connection closed while reading PROXY v1 header".into(),
            ));
        }
        total_read += n;
    }
}

/// Parse a v1 header line like `PROXY TCP4 192.168.1.1 192.168.1.2 12345 80`.
fn parse_v1_line(line: &str) -> Result<ProxyProtocolHeader, ProxyError> {
    let parts: Vec<&str> = line.split_whitespace().collect();

    // Must start with "PROXY"
    if parts.is_empty() || parts[0] != "PROXY" {
        return Err(ProxyError::Internal("invalid PROXY v1 header".into()));
    }

    // PROXY UNKNOWN is valid — means we don't know the addresses.
    if parts.len() >= 2 && parts[1] == "UNKNOWN" {
        return Err(ProxyError::Internal(
            "PROXY v1 UNKNOWN protocol — no address info".into(),
        ));
    }

    if parts.len() < 6 {
        return Err(ProxyError::Internal(format!(
            "PROXY v1 header has too few fields: {line}"
        )));
    }

    let proto = parts[1]; // TCP4 or TCP6
    let src_ip_str = parts[2];
    let dst_ip_str = parts[3];
    let src_port: u16 = parts[4].parse().map_err(|_| {
        ProxyError::Internal(format!("invalid source port in PROXY v1: {}", parts[4]))
    })?;
    let dst_port: u16 = parts[5].parse().map_err(|_| {
        ProxyError::Internal(format!(
            "invalid destination port in PROXY v1: {}",
            parts[5]
        ))
    })?;

    let src_ip: IpAddr = match proto {
        "TCP4" => src_ip_str.parse::<Ipv4Addr>().map(IpAddr::V4),
        "TCP6" => src_ip_str.parse::<Ipv6Addr>().map(IpAddr::V6),
        _ => {
            return Err(ProxyError::Internal(format!(
                "unknown protocol in PROXY v1: {proto}"
            )));
        }
    }
    .map_err(|_| ProxyError::Internal(format!("invalid source IP in PROXY v1: {src_ip_str}")))?;

    let dst_ip: IpAddr = match proto {
        "TCP4" => dst_ip_str.parse::<Ipv4Addr>().map(IpAddr::V4),
        "TCP6" => dst_ip_str.parse::<Ipv6Addr>().map(IpAddr::V6),
        _ => unreachable!(), // handled above
    }
    .map_err(|_| {
        ProxyError::Internal(format!("invalid destination IP in PROXY v1: {dst_ip_str}"))
    })?;

    Ok(ProxyProtocolHeader {
        src_addr: SocketAddr::new(src_ip, src_port),
        dst_addr: Some(SocketAddr::new(dst_ip, dst_port)),
    })
}

/// Parse a PROXY protocol v2 (binary) header.
async fn parse_v2(
    buf: &mut Vec<u8>,
    mut total_read: usize,
    stream: &mut TcpStream,
) -> Result<(Option<ProxyProtocolHeader>, Vec<u8>), ProxyError> {
    // Bytes 12-15:
    //   byte 12: version (upper nibble) | command (lower nibble)
    //   byte 13: address family (upper nibble) | transport protocol (lower nibble)
    //   bytes 14-15: length of the address/TLV block (big-endian u16)

    let ver_cmd = buf[12];
    let version = (ver_cmd >> 4) & 0x0F;
    let command = ver_cmd & 0x0F;

    if version != 2 {
        return Err(ProxyError::Internal(format!(
            "unsupported PROXY v2 version: {version}"
        )));
    }

    let fam_proto = buf[13];
    let family = (fam_proto >> 4) & 0x0F;
    let _transport = fam_proto & 0x0F;

    let addr_len = u16::from_be_bytes([buf[14], buf[15]]) as usize;
    let total_header_len = 16 + addr_len;

    if total_header_len > MAX_HEADER_SIZE {
        return Err(ProxyError::Internal(format!(
            "PROXY v2 header too large: {total_header_len} bytes"
        )));
    }

    // Ensure we have enough data.
    if buf.len() < total_header_len {
        buf.resize(total_header_len, 0);
    }
    while total_read < total_header_len {
        let n = stream.read(&mut buf[total_read..total_header_len]).await?;
        if n == 0 {
            return Err(ProxyError::Internal(
                "connection closed while reading PROXY v2 header".into(),
            ));
        }
        total_read += n;
    }

    // Command 0x00 = LOCAL (health check etc.), 0x01 = PROXY.
    if command == 0x00 {
        debug!("PROXY v2 LOCAL command (no address info)");
        let remaining = buf[total_header_len..total_read].to_vec();
        return Ok((None, remaining));
    }

    if command != 0x01 {
        warn!(command, "unknown PROXY v2 command");
        let remaining = buf[total_header_len..total_read].to_vec();
        return Ok((None, remaining));
    }

    let addr_data = &buf[16..16 + addr_len];

    let header = match family {
        // AF_INET (IPv4)
        0x01 => {
            if addr_len < 12 {
                return Err(ProxyError::Internal("PROXY v2 IPv4 addr too short".into()));
            }
            let src_ip = Ipv4Addr::new(addr_data[0], addr_data[1], addr_data[2], addr_data[3]);
            let dst_ip = Ipv4Addr::new(addr_data[4], addr_data[5], addr_data[6], addr_data[7]);
            let src_port = u16::from_be_bytes([addr_data[8], addr_data[9]]);
            let dst_port = u16::from_be_bytes([addr_data[10], addr_data[11]]);

            ProxyProtocolHeader {
                src_addr: SocketAddr::new(IpAddr::V4(src_ip), src_port),
                dst_addr: Some(SocketAddr::new(IpAddr::V4(dst_ip), dst_port)),
            }
        }
        // AF_INET6 (IPv6)
        0x02 => {
            if addr_len < 36 {
                return Err(ProxyError::Internal("PROXY v2 IPv6 addr too short".into()));
            }
            let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&addr_data[0..16]).unwrap());
            let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&addr_data[16..32]).unwrap());
            let src_port = u16::from_be_bytes([addr_data[32], addr_data[33]]);
            let dst_port = u16::from_be_bytes([addr_data[34], addr_data[35]]);

            ProxyProtocolHeader {
                src_addr: SocketAddr::new(IpAddr::V6(src_ip), src_port),
                dst_addr: Some(SocketAddr::new(IpAddr::V6(dst_ip), dst_port)),
            }
        }
        // AF_UNSPEC
        0x00 => {
            debug!("PROXY v2 AF_UNSPEC — no address info");
            let remaining = buf[total_header_len..total_read].to_vec();
            return Ok((None, remaining));
        }
        _ => {
            warn!(family, "unknown PROXY v2 address family");
            let remaining = buf[total_header_len..total_read].to_vec();
            return Ok((None, remaining));
        }
    };

    debug!(
        src = %header.src_addr,
        dst = ?header.dst_addr,
        "parsed PROXY protocol v2 header"
    );

    let remaining = buf[total_header_len..total_read].to_vec();
    Ok((Some(header), remaining))
}

// ---------------------------------------------------------------------------
// PrefixedStream
// ---------------------------------------------------------------------------

pin_project! {
    /// A wrapper around a `TcpStream` that prepends buffered bytes.
    ///
    /// After parsing the PROXY protocol header, there may be leftover bytes
    /// in our read buffer that belong to the actual application data. This
    /// stream serves those bytes first, then delegates to the inner stream.
    pub struct PrefixedStream {
        prefix: Vec<u8>,
        offset: usize,
        #[pin]
        inner: TcpStream,
    }
}

impl PrefixedStream {
    /// Create a new `PrefixedStream` with the given prefix bytes and inner stream.
    pub fn new(prefix: Vec<u8>, inner: TcpStream) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();

        // Serve from the prefix buffer first.
        if *this.offset < this.prefix.len() {
            let remaining = &this.prefix[*this.offset..];
            let to_copy = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..to_copy]);
            *this.offset += to_copy;
            return Poll::Ready(Ok(()));
        }

        // Prefix exhausted — read from the inner stream.
        this.inner.poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_v1_tcp4_line() {
        let header = parse_v1_line("PROXY TCP4 192.168.1.100 10.0.0.1 56324 443").unwrap();
        assert_eq!(
            header.src_addr,
            "192.168.1.100:56324".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            header.dst_addr,
            Some("10.0.0.1:443".parse::<SocketAddr>().unwrap())
        );
    }

    #[test]
    fn parse_v1_tcp6_line() {
        let header = parse_v1_line("PROXY TCP6 2001:db8::1 2001:db8::2 56324 443").unwrap();
        assert_eq!(
            header.src_addr,
            "[2001:db8::1]:56324".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            header.dst_addr,
            Some("[2001:db8::2]:443".parse::<SocketAddr>().unwrap())
        );
    }

    #[test]
    fn parse_v1_unknown_is_error() {
        let result = parse_v1_line("PROXY UNKNOWN");
        assert!(result.is_err());
    }

    #[test]
    fn parse_v1_too_few_fields() {
        let result = parse_v1_line("PROXY TCP4 1.2.3.4 5.6.7.8 1234");
        assert!(result.is_err());
    }

    #[test]
    fn v2_signature_constant() {
        assert_eq!(V2_SIGNATURE.len(), 12);
        assert_eq!(V2_SIGNATURE[0], b'\r');
        assert_eq!(V2_SIGNATURE[11], b'\n');
    }
}
