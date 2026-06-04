//! BOOTP/DHCP packet parse + encode. Pure, no I/O. The parser returns `Result`
//! and never panics on malformed input.

use std::net::Ipv4Addr;

/// DHCP magic cookie (RFC 2131) that precedes the options area.
pub const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// BOOTP `op` for a client request.
pub const BOOTREQUEST: u8 = 1;
/// BOOTP `op` for a server reply.
pub const BOOTREPLY: u8 = 2;

/// Offset of the magic cookie; the fixed BOOTP header is `[0, 236)`.
const COOKIE_OFFSET: usize = 236;
/// Minimum valid packet: fixed header + cookie.
const MIN_LEN: usize = 240;

/// A parsed BOOTP/DHCP message. The options area is kept as raw TLVs; typed
/// access lives in `options.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpMessage {
    pub op: u8,
    pub htype: u8,
    pub hlen: u8,
    pub hops: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub options: Vec<DhcpOption>,
}

/// One DHCP option as a code + raw value (TLV). PAD (0) and END (255) are not
/// stored; END terminates parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpOption {
    pub code: u8,
    pub data: Vec<u8>,
}

/// Error returned by [`parse`]. All variants are recoverable: the caller drops
/// the datagram and keeps serving.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("packet too short: {0} bytes")]
    TooShort(usize),
    #[error("bad magic cookie")]
    BadCookie,
    #[error("truncated option (code {code})")]
    TruncatedOption { code: u8 },
}

fn ipv4(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

/// Parse a datagram into a [`DhcpMessage`]. Never panics.
pub fn parse(buf: &[u8]) -> Result<DhcpMessage, ParseError> {
    if buf.len() < MIN_LEN {
        return Err(ParseError::TooShort(buf.len()));
    }
    if buf[COOKIE_OFFSET..MIN_LEN] != MAGIC_COOKIE {
        return Err(ParseError::BadCookie);
    }
    let mut chaddr = [0u8; 16];
    chaddr.copy_from_slice(&buf[28..44]);
    Ok(DhcpMessage {
        op: buf[0],
        htype: buf[1],
        hlen: buf[2],
        hops: buf[3],
        xid: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        secs: u16::from_be_bytes([buf[8], buf[9]]),
        flags: u16::from_be_bytes([buf[10], buf[11]]),
        ciaddr: ipv4(&buf[12..16]),
        yiaddr: ipv4(&buf[16..20]),
        siaddr: ipv4(&buf[20..24]),
        giaddr: ipv4(&buf[24..28]),
        chaddr,
        options: parse_options(&buf[MIN_LEN..])?,
    })
}

fn parse_options(buf: &[u8]) -> Result<Vec<DhcpOption>, ParseError> {
    let mut opts = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let code = buf[i];
        match code {
            0 => {
                i += 1;
                continue;
            } // PAD
            255 => break, // END
            _ => {}
        }
        if i + 1 >= buf.len() {
            return Err(ParseError::TruncatedOption { code });
        }
        let len = buf[i + 1] as usize;
        let start = i + 2;
        let end = start + len;
        if end > buf.len() {
            return Err(ParseError::TruncatedOption { code });
        }
        opts.push(DhcpOption {
            code,
            data: buf[start..end].to_vec(),
        });
        i = end;
    }
    Ok(opts)
}

/// Encode a [`DhcpMessage`] into wire bytes (fixed header + cookie + TLV options + END).
pub fn encode(msg: &DhcpMessage) -> Vec<u8> {
    let mut out = vec![0u8; MIN_LEN];
    out[0] = msg.op;
    out[1] = msg.htype;
    out[2] = msg.hlen;
    out[3] = msg.hops;
    out[4..8].copy_from_slice(&msg.xid.to_be_bytes());
    out[8..10].copy_from_slice(&msg.secs.to_be_bytes());
    out[10..12].copy_from_slice(&msg.flags.to_be_bytes());
    out[12..16].copy_from_slice(&msg.ciaddr.octets());
    out[16..20].copy_from_slice(&msg.yiaddr.octets());
    out[20..24].copy_from_slice(&msg.siaddr.octets());
    out[24..28].copy_from_slice(&msg.giaddr.octets());
    out[28..44].copy_from_slice(&msg.chaddr);
    out[COOKIE_OFFSET..MIN_LEN].copy_from_slice(&MAGIC_COOKIE);
    for opt in &msg.options {
        debug_assert!(opt.data.len() <= 255, "option {} data exceeds 255 bytes", opt.code);
        out.push(opt.code);
        out.push(opt.data.len() as u8);
        out.extend_from_slice(&opt.data);
    }
    out.push(255); // END
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sample() -> DhcpMessage {
        DhcpMessage {
            op: BOOTREQUEST,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 0xDEAD_BEEF,
            secs: 0,
            flags: 0x8000,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::new(10, 0, 0, 1),
            chaddr: [
                0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
            options: vec![
                DhcpOption {
                    code: 53,
                    data: vec![1],
                },
                DhcpOption {
                    code: 60,
                    data: b"PXEClient".to_vec(),
                },
            ],
        }
    }

    #[test]
    fn round_trip_message() {
        let msg = sample();
        let bytes = encode(&msg);
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn rejects_short_packet() {
        assert_eq!(parse(&[0u8; 10]), Err(ParseError::TooShort(10)));
    }

    #[test]
    fn rejects_bad_cookie() {
        let mut bytes = encode(&sample());
        bytes[236] = 0; // corrupt the magic cookie
        assert_eq!(parse(&bytes), Err(ParseError::BadCookie));
    }

    #[test]
    fn rejects_truncated_option() {
        let mut bytes = encode(&sample());
        // Append an option claiming 5 bytes but supplying none.
        bytes.truncate(240);
        bytes.extend_from_slice(&[99, 5]); // code 99, len 5, no data
        assert_eq!(parse(&bytes), Err(ParseError::TruncatedOption { code: 99 }));
    }

    #[test]
    fn skips_pad_and_stops_at_end() {
        let mut bytes = vec![0u8; 240];
        bytes[0] = BOOTREQUEST;
        bytes[236..240].copy_from_slice(&MAGIC_COOKIE);
        bytes.extend_from_slice(&[0, 0, 53, 1, 1, 255, 0, 0]); // PADs, opt53=DISCOVER, END, trailing
        let parsed = parse(&bytes).unwrap();
        assert_eq!(
            parsed.options,
            vec![DhcpOption {
                code: 53,
                data: vec![1]
            }]
        );
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
            let _ = parse(&bytes);
        }
    }
}
