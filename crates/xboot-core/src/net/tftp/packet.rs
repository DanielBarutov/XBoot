//! TFTP packet parse + encode (RFC 1350 + options RFC 2347/2348/2349).
//! Pure, no I/O. The parser returns `Result` and never panics.

/// TFTP opcode constants (RFC 1350).
pub const OP_RRQ: u16 = 1;
pub const OP_WRQ: u16 = 2;
pub const OP_DATA: u16 = 3;
pub const OP_ACK: u16 = 4;
pub const OP_ERROR: u16 = 5;
pub const OP_OACK: u16 = 6;

/// TFTP error codes (RFC 1350 §5, RFC 2347 §3).
pub const ERR_UNDEFINED: u16 = 0;
pub const ERR_FILE_NOT_FOUND: u16 = 1;
pub const ERR_ACCESS_VIOLATION: u16 = 2;

/// A parsed inbound TFTP packet (what the server receives).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// Read request: opcode 1.
    Rrq {
        filename: String,
        mode: String,
        /// Any trailing options as (key, value) pairs.
        options: Vec<(String, String)>,
    },
    /// Write request: opcode 2. Server responds with error; we still parse it.
    Wrq { filename: String, mode: String },
    /// Acknowledgement: opcode 4, block number.
    Ack { block: u16 },
    /// Error: opcode 5.
    Error { code: u16, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Packet too short to contain even the opcode.
    TooShort,
    /// Opcode is not one of 1–6.
    UnknownOpcode(u16),
    /// A null-terminated field is missing its terminator.
    MissingNull,
    /// Filename or mode is empty.
    EmptyField,
    /// ACK packet has wrong payload length (must be exactly 4 bytes: opcode + block).
    BadAckLength(usize),
    /// Could not parse option value as a number.
    BadOptionValue,
}

/// Parse an inbound TFTP datagram. Returns a `Packet` variant or a `ParseError`.
/// Never panics on arbitrary input.
pub fn parse(buf: &[u8]) -> Result<Packet, ParseError> {
    if buf.len() < 2 {
        return Err(ParseError::TooShort);
    }
    let opcode = u16::from_be_bytes([buf[0], buf[1]]);
    match opcode {
        OP_RRQ | OP_WRQ => parse_rq(opcode, &buf[2..]),
        OP_ACK => parse_ack(&buf[2..]),
        OP_ERROR => parse_error(&buf[2..]),
        _ => Err(ParseError::UnknownOpcode(opcode)),
    }
}

/// Extract null-terminated strings from `buf` starting at `pos`.
/// Returns (string, new_position) or MissingNull if no null terminator found.
fn read_cstr(buf: &[u8], pos: usize) -> Result<(String, usize), ParseError> {
    let end = buf[pos..]
        .iter()
        .position(|&b| b == 0)
        .ok_or(ParseError::MissingNull)?;
    let abs_end = pos + end;
    let s = String::from_utf8_lossy(&buf[pos..abs_end]).into_owned();
    if s.is_empty() {
        return Err(ParseError::EmptyField);
    }
    Ok((s, abs_end + 1)) // +1 to skip the null
}

fn parse_rq(opcode: u16, tail: &[u8]) -> Result<Packet, ParseError> {
    let (filename, pos) = read_cstr(tail, 0)?;
    let (mode, mut pos) = read_cstr(tail, pos)?;

    // Parse optional key-value pairs (only for RRQ; WRQ stops here).
    if opcode == OP_WRQ {
        return Ok(Packet::Wrq { filename, mode });
    }

    let mut options: Vec<(String, String)> = Vec::new();
    while pos < tail.len() {
        let (key, new_pos) = read_cstr(tail, pos)?;
        if new_pos >= tail.len() {
            // Key without a value — ignore (truncated option pair).
            break;
        }
        let (value, new_pos) = read_cstr(tail, new_pos)?;
        pos = new_pos;
        options.push((key.to_lowercase(), value));
    }

    Ok(Packet::Rrq {
        filename,
        mode,
        options,
    })
}

fn parse_ack(tail: &[u8]) -> Result<Packet, ParseError> {
    if tail.len() != 2 {
        return Err(ParseError::BadAckLength(tail.len() + 2));
    }
    let block = u16::from_be_bytes([tail[0], tail[1]]);
    Ok(Packet::Ack { block })
}

fn parse_error(tail: &[u8]) -> Result<Packet, ParseError> {
    if tail.len() < 3 {
        // minimum: 2-byte error code + at least \0
        return Err(ParseError::TooShort);
    }
    let code = u16::from_be_bytes([tail[0], tail[1]]);
    let message = String::from_utf8_lossy(
        &tail[2..tail.len().saturating_sub(1)], // drop trailing null if present
    )
    .into_owned();
    Ok(Packet::Error { code, message })
}

/// Returns `true` if `name` is a safe relative path that does not escape its
/// intended directory. Rejects absolute paths, `..` components, and empty names.
pub fn is_safe_path(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // Reject absolute paths (Unix and Windows)
    if name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    // Reject Windows drive-letter paths: "C:..." where ':' is at position 1
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return false;
    }
    for component in name.split(['/', '\\']) {
        if component == ".." || component == "." {
            return false;
        }
    }
    true
}

/// Encode a DATA packet (opcode 3): opcode + block number + payload.
/// Block numbers start at 1 (RFC 1350) or 0 for the OACK acceptance case.
pub fn encode_data(block: u16, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&OP_DATA.to_be_bytes());
    buf.extend_from_slice(&block.to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

/// Encode an OACK packet (opcode 6): opcode + null-terminated key-value pairs.
pub fn encode_oack(options: &[(String, String)]) -> Vec<u8> {
    let mut buf = vec![];
    buf.extend_from_slice(&OP_OACK.to_be_bytes());
    for (key, value) in options {
        buf.extend_from_slice(key.as_bytes());
        buf.push(0);
        buf.extend_from_slice(value.as_bytes());
        buf.push(0);
    }
    buf
}

/// Encode an ERROR packet (opcode 5): opcode + error code + null-terminated message.
pub fn encode_error(code: u16, message: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + message.len() + 1);
    buf.extend_from_slice(&OP_ERROR.to_be_bytes());
    buf.extend_from_slice(&code.to_be_bytes());
    buf.extend_from_slice(message.as_bytes());
    buf.push(0);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- RRQ ---

    #[test]
    fn parse_rrq_no_options() {
        let mut buf = vec![0x00, 0x01]; // opcode RRQ
        buf.extend(b"undionly.kpxe\0");
        buf.extend(b"octet\0");
        let pkt = parse(&buf).unwrap();
        assert_eq!(
            pkt,
            Packet::Rrq {
                filename: "undionly.kpxe".into(),
                mode: "octet".into(),
                options: vec![],
            }
        );
    }

    #[test]
    fn parse_rrq_with_blksize() {
        let mut buf = vec![0x00, 0x01];
        buf.extend(b"ipxe.efi\0");
        buf.extend(b"octet\0");
        buf.extend(b"blksize\0");
        buf.extend(b"1468\0");
        let pkt = parse(&buf).unwrap();
        assert_eq!(
            pkt,
            Packet::Rrq {
                filename: "ipxe.efi".into(),
                mode: "octet".into(),
                options: vec![("blksize".into(), "1468".into())],
            }
        );
    }

    #[test]
    fn parse_rrq_with_tsize() {
        let mut buf = vec![0x00, 0x01];
        buf.extend(b"boot.kpxe\0");
        buf.extend(b"octet\0");
        buf.extend(b"tsize\0");
        buf.extend(b"0\0");
        let pkt = parse(&buf).unwrap();
        assert_eq!(
            pkt,
            Packet::Rrq {
                filename: "boot.kpxe".into(),
                mode: "octet".into(),
                options: vec![("tsize".into(), "0".into())],
            }
        );
    }

    #[test]
    fn parse_rrq_multiple_options() {
        let mut buf = vec![0x00, 0x01];
        buf.extend(b"file\0");
        buf.extend(b"octet\0");
        buf.extend(b"blksize\0");
        buf.extend(b"1468\0");
        buf.extend(b"tsize\0");
        buf.extend(b"0\0");
        buf.extend(b"timeout\0");
        buf.extend(b"5\0");
        let pkt = parse(&buf).unwrap();
        assert_eq!(
            pkt,
            Packet::Rrq {
                filename: "file".into(),
                mode: "octet".into(),
                options: vec![
                    ("blksize".into(), "1468".into()),
                    ("tsize".into(), "0".into()),
                    ("timeout".into(), "5".into()),
                ],
            }
        );
    }

    // --- WRQ ---

    #[test]
    fn parse_wrq() {
        let mut buf = vec![0x00, 0x02];
        buf.extend(b"write.me\0");
        buf.extend(b"octet\0");
        let pkt = parse(&buf).unwrap();
        assert_eq!(
            pkt,
            Packet::Wrq {
                filename: "write.me".into(),
                mode: "octet".into(),
            }
        );
    }

    // --- ACK ---

    #[test]
    fn parse_ack() {
        let buf = [0x00, 0x04, 0x00, 0x01]; // ACK block 1
        let pkt = parse(&buf).unwrap();
        assert_eq!(pkt, Packet::Ack { block: 1 });
    }

    #[test]
    fn parse_ack_zero() {
        let buf = [0x00, 0x04, 0x00, 0x00]; // ACK block 0 (OACK accept)
        let pkt = parse(&buf).unwrap();
        assert_eq!(pkt, Packet::Ack { block: 0 });
    }

    #[test]
    fn parse_ack_max_block() {
        let buf = [0x00, 0x04, 0xFF, 0xFF]; // ACK block 65535
        let pkt = parse(&buf).unwrap();
        assert_eq!(pkt, Packet::Ack { block: 65535 });
    }

    // --- ERROR ---

    #[test]
    fn parse_error() {
        let mut buf = vec![0x00, 0x05, 0x00, 0x01]; // ERROR code 1
        buf.extend(b"File not found\0");
        let pkt = parse(&buf).unwrap();
        assert_eq!(
            pkt,
            Packet::Error {
                code: 1,
                message: "File not found".into(),
            }
        );
    }

    // --- Rejections ---

    #[test]
    fn reject_too_short() {
        assert!(matches!(parse(&[0x00]), Err(ParseError::TooShort)));
    }

    #[test]
    fn reject_unknown_opcode() {
        let buf = [0x00, 0xFF, 0x00];
        assert!(matches!(parse(&buf), Err(ParseError::UnknownOpcode(0xFF))));
    }

    #[test]
    fn reject_rrq_missing_mode_null() {
        let mut buf = vec![0x00, 0x01];
        buf.extend(b"file\0");
        buf.extend(b"octet"); // no trailing null
        assert!(matches!(parse(&buf), Err(ParseError::MissingNull)));
    }

    #[test]
    fn reject_rrq_empty_filename() {
        let buf = [0x00, 0x01, 0x00, b'o', b'c', b't', b'e', b't', 0x00];
        assert!(matches!(parse(&buf), Err(ParseError::EmptyField)));
    }

    #[test]
    fn reject_ack_bad_length() {
        let buf = [0x00, 0x04, 0x00]; // 3 bytes: opcode + one payload byte
        assert!(matches!(parse(&buf), Err(ParseError::BadAckLength(3))));
    }

    #[test]
    fn reject_ack_too_long() {
        let buf = [0x00, 0x04, 0x00, 0x01, 0xFF]; // 5 bytes
        assert!(matches!(parse(&buf), Err(ParseError::BadAckLength(5))));
    }

    // --- Encoder tests ---

    #[test]
    fn encode_data_block() {
        let data = vec![0xAA; 512];
        let pkt = encode_data(1, &data);
        assert_eq!(&pkt[0..2], &[0x00, 0x03]); // opcode DATA
        assert_eq!(&pkt[2..4], &[0x00, 0x01]); // block 1
        assert_eq!(&pkt[4..], &data[..]); // payload
        assert_eq!(pkt.len(), 4 + 512);
    }

    #[test]
    fn encode_data_last_block() {
        // Last block is < 512 bytes
        let data = vec![0xBB; 100];
        let pkt = encode_data(42, &data);
        assert_eq!(&pkt[0..2], &[0x00, 0x03]);
        assert_eq!(&pkt[2..4], &[0x00, 0x2A]); // block 42 = 0x002A
        assert_eq!(pkt.len(), 4 + 100);
    }

    #[test]
    fn encode_data_block_65535() {
        let pkt = encode_data(65535, b"x");
        assert_eq!(&pkt[0..2], &[0x00, 0x03]);
        assert_eq!(&pkt[2..4], &[0xFF, 0xFF]); // max block number
    }

    #[test]
    fn encode_oack_with_options() {
        let opts = vec![
            ("blksize".to_string(), "1468".to_string()),
            ("tsize".to_string(), "284672".to_string()),
        ];
        let pkt = encode_oack(&opts);
        assert_eq!(&pkt[0..2], &[0x00, 0x06]); // opcode OACK
        let rest = &pkt[2..];
        // Null-terminated key-value pairs
        let expected = b"blksize\x001468\x00tsize\x00284672\x00";
        assert_eq!(rest, expected);
    }

    #[test]
    fn encode_oack_empty_options() {
        let pkt = encode_oack(&[]);
        assert_eq!(&pkt[0..2], &[0x00, 0x06]);
        assert_eq!(pkt.len(), 2); // just the opcode
    }

    #[test]
    fn encode_error_test() {
        let pkt = encode_error(1, "File not found");
        assert_eq!(&pkt[0..2], &[0x00, 0x05]); // opcode ERROR
        assert_eq!(&pkt[2..4], &[0x00, 0x01]); // code 1
        assert_eq!(&pkt[4..], b"File not found\0");
    }

    // --- Path safety tests ---

    #[test]
    fn safe_relative_path() {
        assert!(is_safe_path("undionly.kpxe"));
        assert!(is_safe_path("subdir/ipxe.efi"));
        assert!(is_safe_path("a/b/c/file.bin"));
    }

    #[test]
    fn reject_absolute_unix_path() {
        assert!(!is_safe_path("/etc/passwd"));
        assert!(!is_safe_path("/srv/xboot/file"));
    }

    #[test]
    fn reject_parent_traversal() {
        assert!(!is_safe_path("../secret"));
        assert!(!is_safe_path("subdir/../../etc/passwd"));
        assert!(!is_safe_path(".."));
    }

    #[test]
    fn reject_absolute_windows_path() {
        assert!(!is_safe_path("\\Windows\\secret"));
        assert!(!is_safe_path("D:\\xboot\\file"));
    }

    #[test]
    fn reject_current_dir_segments() {
        // "." components are unnecessary for our use case and risky
        assert!(!is_safe_path("./file"));
        assert!(!is_safe_path("subdir/./file"));
    }
}
