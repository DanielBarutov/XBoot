use std::io;

use crate::storage::invalid_data;

/// Disk types we care about.
pub(crate) const DISK_TYPE_FIXED: u32 = 2;
#[allow(dead_code)]
pub(crate) const DISK_TYPE_DYNAMIC: u32 = 3;

/// The parsed fields of a 512-byte VHD footer that we use.
pub(crate) struct VhdFooter {
    #[allow(dead_code)] // used in Task 9 (dynamic VHD)
    pub data_offset: u64,
    pub current_size: u64,
    pub disk_type: u32,
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl VhdFooter {
    /// Parse and validate a footer from at least 512 bytes.
    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 512 {
            return Err(invalid_data("VHD footer shorter than 512 bytes"));
        }
        let f = &bytes[..512];
        if &f[0..8] != b"conectix" {
            return Err(invalid_data("VHD footer cookie mismatch"));
        }

        let stored = be_u32(&f[64..68]);
        let mut sum: u32 = 0;
        for (i, &b) in f.iter().enumerate() {
            if (64..68).contains(&i) {
                continue;
            }
            sum = sum.wrapping_add(b as u32);
        }
        if !sum != stored {
            return Err(invalid_data("VHD footer checksum mismatch"));
        }

        Ok(Self {
            data_offset: be_u64(&f[16..24]),
            current_size: be_u64(&f[48..56]),
            disk_type: be_u32(&f[60..64]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;

    #[test]
    fn parses_fixed_footer() {
        let f = fixtures::footer(2, 4096, u64::MAX);
        let parsed = VhdFooter::parse(&f).unwrap();
        assert_eq!(parsed.disk_type, 2);
        assert_eq!(parsed.current_size, 4096);
        assert_eq!(parsed.data_offset, u64::MAX);
    }

    #[test]
    fn parses_dynamic_footer() {
        let f = fixtures::footer(3, 8192, 512);
        let parsed = VhdFooter::parse(&f).unwrap();
        assert_eq!(parsed.disk_type, 3);
        assert_eq!(parsed.data_offset, 512);
    }

    #[test]
    fn rejects_bad_cookie() {
        let mut f = fixtures::footer(2, 4096, u64::MAX);
        f[0] = b'X';
        assert!(VhdFooter::parse(&f).is_err());
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut f = fixtures::footer(2, 4096, u64::MAX);
        f[100] ^= 0xFF; // corrupt a byte the checksum covers
        assert!(VhdFooter::parse(&f).is_err());
    }

    #[test]
    fn rejects_short_input() {
        assert!(VhdFooter::parse(&[0u8; 100]).is_err());
    }
}
