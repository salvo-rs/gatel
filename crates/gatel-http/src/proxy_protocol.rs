//! The PROXY protocol: who the client really is, behind a layer-4 balancer.
//!
//! An AWS Network Load Balancer, a HAProxy in TCP mode, a GCP internal load
//! balancer — none of them can add a forwarding header, because none of them
//! reads the request. The connection arrives with their address on it and the
//! client's address nowhere. So they send it first, in a small header, before
//! anything else on the socket.
//!
//! Without this a deployment behind one of those does not merely lose a nicety
//! in the access log: `ip-filter` matches the balancer, a per-client
//! `rate-limit` counts every client as one client, and `X-Forwarded-For` names
//! the wrong machine. The data is wrong in the three places where being wrong
//! matters most.
//!
//! Two versions, and both are needed. Version 1 is a line of text and is what
//! older HAProxy and stunnel send; version 2 is binary and is what an AWS NLB
//! sends. Reading them is not optional-feature territory — a listener told to
//! expect the header must find one, because the bytes are on the socket ahead
//! of the request either way.
//!
//! # Trust
//!
//! Anyone who can reach the port can claim any address. That is not a flaw in
//! the protocol; it is why it is only ever spoken on a port reachable by the
//! balancer. Gatel enforces the same rule it already applies to forwarding
//! headers: the header is read only when the peer is a configured
//! `trusted-proxy`, and a connection from anywhere else is closed rather than
//! served. Compiling a listener with `proxy-protocol` and no `trusted-proxy`
//! is an error, because it would mean believing everybody.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

/// The v2 header begins with these twelve bytes, which are not valid at the
/// start of any HTTP request.
const SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// The longest a version 1 line can be, including its terminator, from the
/// specification. A line that has not ended by then is not a header.
const V1_MAX: usize = 107;

/// What a header said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The connection is on behalf of this address.
    Client(SocketAddr),
    /// The balancer is talking to us for itself — a health check. There is no
    /// client, so the peer address stands.
    Local,
}

/// Read the header from the front of a connection.
///
/// Reads exactly the header and not one byte more, so what remains on the
/// socket is the request, untouched, for whatever speaks next — TLS or HTTP.
///
/// The timeout matters: a connection that opens and then says nothing must not
/// hold a slot, and this runs before the deadline that bounds a request head
/// would otherwise apply.
pub async fn read<IO>(stream: &mut IO, timeout: Duration) -> Result<Claim, String>
where
    IO: AsyncRead + Unpin,
{
    match tokio::time::timeout(timeout, parse(stream)).await {
        Ok(result) => result,
        Err(_) => Err(format!("no PROXY header within {timeout:?}")),
    }
}

async fn parse<IO>(stream: &mut IO) -> Result<Claim, String>
where
    IO: AsyncRead + Unpin,
{
    // Twelve bytes decide the version, and are useful either way: they are the
    // v2 signature, or the first twelve characters of the v1 line.
    let mut head = [0u8; SIGNATURE.len()];
    stream
        .read_exact(&mut head)
        .await
        .map_err(|error| format!("the connection ended before a PROXY header: {error}"))?;

    if head == SIGNATURE {
        return version_2(stream).await;
    }
    version_1(stream, &head).await
}

/// `PROXY TCP4 192.0.2.1 198.51.100.1 56324 443\r\n`
async fn version_1<IO>(stream: &mut IO, seen: &[u8]) -> Result<Claim, String>
where
    IO: AsyncRead + Unpin,
{
    if !seen.starts_with(b"PROXY ") {
        return Err("the connection did not begin with a PROXY header".to_string());
    }

    // One byte at a time, because the line is the only thing that says where it
    // ends and reading past it would eat the request. It is at most 107 bytes.
    let mut line = seen.to_vec();
    loop {
        if line.len() > V1_MAX {
            return Err("the PROXY header line is longer than the protocol allows".to_string());
        }
        let mut byte = [0u8; 1];
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|error| format!("the PROXY header line did not end: {error}"))?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            break;
        }
    }

    let line = std::str::from_utf8(&line[..line.len() - 2])
        .map_err(|_| "the PROXY header line is not text".to_string())?;
    let mut parts = line.split(' ');
    let _ = parts.next(); // PROXY

    match parts.next() {
        // "UNKNOWN" means the balancer could not work out the source. The rest
        // of the line may be anything, and is ignored.
        Some("UNKNOWN") => Ok(Claim::Local),
        Some(family @ ("TCP4" | "TCP6")) => {
            let source = parts.next().ok_or("the PROXY header has no source address")?;
            let _destination = parts.next().ok_or("the PROXY header has no destination")?;
            let port = parts.next().ok_or("the PROXY header has no source port")?;
            if parts.next().is_none() {
                return Err("the PROXY header has no destination port".to_string());
            }
            if parts.next().is_some() {
                return Err("the PROXY header line has trailing text".to_string());
            }

            let address: IpAddr = source
                .parse()
                .map_err(|_| format!("`{source}` is not an address"))?;
            if (family == "TCP4") != address.is_ipv4() {
                return Err(format!("the PROXY header says {family} and carries {source}"));
            }
            let port: u16 = port.parse().map_err(|_| format!("`{port}` is not a port"))?;
            Ok(Claim::Client(SocketAddr::new(address, port)))
        }
        other => Err(format!(
            "the PROXY header names an unknown transport `{}`",
            other.unwrap_or("")
        )),
    }
}

/// The binary form: four more header bytes, then an address block whose length
/// the header states.
async fn version_2<IO>(stream: &mut IO) -> Result<Claim, String>
where
    IO: AsyncRead + Unpin,
{
    let mut rest = [0u8; 4];
    stream
        .read_exact(&mut rest)
        .await
        .map_err(|error| format!("the PROXY v2 header is incomplete: {error}"))?;

    let version = rest[0] >> 4;
    if version != 2 {
        return Err(format!("PROXY header version {version} is not supported"));
    }
    let command = rest[0] & 0x0F;
    let family = rest[1] >> 4;
    let length = u16::from_be_bytes([rest[2], rest[3]]) as usize;

    // Read the block whatever it holds, because the bytes are on the socket
    // and leaving them there would feed them to the request parser.
    let mut block = vec![0u8; length];
    stream
        .read_exact(&mut block)
        .await
        .map_err(|error| format!("the PROXY v2 address block is incomplete: {error}"))?;

    match command {
        // LOCAL: the balancer's own connection, such as a health check.
        0 => return Ok(Claim::Local),
        1 => {}
        other => return Err(format!("PROXY v2 command {other} is not supported")),
    }

    match family {
        // AF_UNSPEC, and AF_UNIX, carry no address we can use.
        0 | 3 => Ok(Claim::Local),
        1 => {
            if block.len() < 12 {
                return Err("the PROXY v2 IPv4 block is too short".to_string());
            }
            let source = Ipv4Addr::new(block[0], block[1], block[2], block[3]);
            let port = u16::from_be_bytes([block[8], block[9]]);
            Ok(Claim::Client(SocketAddr::new(IpAddr::V4(source), port)))
        }
        2 => {
            if block.len() < 36 {
                return Err("the PROXY v2 IPv6 block is too short".to_string());
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&block[0..16]);
            let port = u16::from_be_bytes([block[32], block[33]]);
            Ok(Claim::Client(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(octets)),
                port,
            )))
        }
        other => Err(format!("PROXY v2 address family {other} is not supported")),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const PATIENT: Duration = Duration::from_secs(5);

    async fn read_all(bytes: &[u8]) -> (Result<Claim, String>, Vec<u8>) {
        let mut cursor = Cursor::new(bytes.to_vec());
        let claim = read(&mut cursor, PATIENT).await;
        let consumed = cursor.position() as usize;
        (claim, bytes[consumed..].to_vec())
    }

    fn v2(command: u8, family: u8, block: &[u8]) -> Vec<u8> {
        let mut bytes = SIGNATURE.to_vec();
        bytes.push(0x20 | command);
        bytes.push(family << 4 | 0x01);
        bytes.extend_from_slice(&(block.len() as u16).to_be_bytes());
        bytes.extend_from_slice(block);
        bytes
    }

    #[tokio::test]
    async fn version_1_ipv4() {
        let (claim, rest) =
            read_all(b"PROXY TCP4 192.0.2.1 198.51.100.1 56324 443\r\nGET / HTTP/1.1\r\n").await;
        assert_eq!(claim.unwrap(), Claim::Client("192.0.2.1:56324".parse().unwrap()));
        // The request must be left exactly as it was: one byte over-read here
        // is a malformed request for every connection.
        assert_eq!(rest, b"GET / HTTP/1.1\r\n");
    }

    #[tokio::test]
    async fn version_1_ipv6() {
        let (claim, rest) = read_all(b"PROXY TCP6 2001:db8::1 2001:db8::2 56324 443\r\nGET /\r\n").await;
        assert_eq!(
            claim.unwrap(),
            Claim::Client("[2001:db8::1]:56324".parse().unwrap())
        );
        assert_eq!(rest, b"GET /\r\n");
    }

    #[tokio::test]
    async fn version_1_unknown_is_the_balancers_own_connection() {
        let (claim, rest) = read_all(b"PROXY UNKNOWN\r\nGET /\r\n").await;
        assert_eq!(claim.unwrap(), Claim::Local);
        assert_eq!(rest, b"GET /\r\n");
    }

    #[tokio::test]
    async fn version_2_ipv4() {
        // What an AWS Network Load Balancer sends.
        let mut block = Vec::new();
        block.extend_from_slice(&[192, 0, 2, 1]); // source
        block.extend_from_slice(&[198, 51, 100, 1]); // destination
        block.extend_from_slice(&56324u16.to_be_bytes());
        block.extend_from_slice(&443u16.to_be_bytes());
        let mut bytes = v2(1, 1, &block);
        bytes.extend_from_slice(b"GET / HTTP/1.1\r\n");

        let (claim, rest) = read_all(&bytes).await;
        assert_eq!(claim.unwrap(), Claim::Client("192.0.2.1:56324".parse().unwrap()));
        assert_eq!(rest, b"GET / HTTP/1.1\r\n");
    }

    #[tokio::test]
    async fn version_2_ipv6() {
        let mut block = Vec::new();
        block.extend_from_slice(&Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets());
        block.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        block.extend_from_slice(&51000u16.to_be_bytes());
        block.extend_from_slice(&443u16.to_be_bytes());
        let bytes = v2(1, 2, &block);

        assert_eq!(
            read_all(&bytes).await.0.unwrap(),
            Claim::Client("[2001:db8::1]:51000".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn a_v2_health_check_carries_no_client() {
        // LOCAL, with a full address block that must still be consumed or the
        // request parser would read it.
        let mut bytes = v2(0, 1, &[0u8; 12]);
        bytes.extend_from_slice(b"GET /\r\n");
        let (claim, rest) = read_all(&bytes).await;
        assert_eq!(claim.unwrap(), Claim::Local);
        assert_eq!(rest, b"GET /\r\n");
    }

    #[tokio::test]
    async fn an_unspecified_family_carries_no_client() {
        let bytes = v2(1, 0, &[]);
        assert_eq!(read_all(&bytes).await.0.unwrap(), Claim::Local);
    }

    #[tokio::test]
    async fn a_plain_request_is_refused_rather_than_misread() {
        // The listener was told every connection carries a header. One that
        // does not is a misconfiguration, and serving it anyway would attribute
        // the request to whatever the first line happened to parse as.
        let (claim, _) = read_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
        assert!(
            claim.unwrap_err().contains("did not begin with a PROXY header"),
            "a plain request must be refused"
        );
    }

    #[tokio::test]
    async fn a_line_that_never_ends_is_refused() {
        let mut bytes = b"PROXY TCP4 ".to_vec();
        bytes.extend(std::iter::repeat_n(b'1', 400));
        assert!(
            read_all(&bytes)
                .await
                .0
                .unwrap_err()
                .contains("longer than the protocol allows")
        );
    }

    #[tokio::test]
    async fn a_family_that_disagrees_with_its_address_is_refused() {
        // `TCP4` carrying an IPv6 address is either a broken balancer or
        // somebody probing; either way it is not something to believe.
        let (claim, _) = read_all(b"PROXY TCP4 2001:db8::1 2001:db8::2 1 2\r\n").await;
        assert!(claim.unwrap_err().contains("says TCP4"));
    }

    #[tokio::test]
    async fn a_truncated_header_is_refused_rather_than_half_believed() {
        for truncated in [
            b"PROXY TCP4 192.0.2.1 198.51.100.1 56324\r\n".as_slice(),
            b"PROXY TCP4 192.0.2.1\r\n".as_slice(),
            b"PROXY\r\n".as_slice(),
            b"PROX".as_slice(),
        ] {
            assert!(read_all(truncated).await.0.is_err(), "{truncated:?}");
        }
    }

    #[tokio::test]
    async fn a_port_that_is_not_a_number_is_refused() {
        let (claim, _) = read_all(b"PROXY TCP4 192.0.2.1 198.51.100.1 https 443\r\n").await;
        assert!(claim.unwrap_err().contains("is not a port"));
    }

    #[tokio::test]
    async fn a_connection_that_says_nothing_times_out() {
        // Otherwise a slot is held by a peer that has done nothing at all, and
        // the deadline that bounds a request head has not started yet.
        struct Silent;
        impl AsyncRead for Silent {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Pending
            }
        }
        let error = read(&mut Silent, Duration::from_millis(50)).await.unwrap_err();
        assert!(error.contains("no PROXY header within"), "{error}");
    }
}
