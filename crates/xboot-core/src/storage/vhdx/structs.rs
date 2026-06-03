use std::io;

use crate::storage::invalid_data;

use super::crc32c::crc32c;

pub(crate) const BAT_GUID: [u8; 16] = [
    0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08,
];
pub(crate) const METADATA_GUID: [u8; 16] = [
    0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E,
];

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

pub(crate) struct VhdxHeader {
    pub sequence: u64,
}

/// Parse one 4096-byte header if its signature, version, CRC, and empty-log
/// invariant hold.
fn parse_one_header(h: &[u8]) -> io::Result<VhdxHeader> {
    if h.len() < 4096 || &h[0..4] != b"head" {
        return Err(invalid_data("VHDX header signature mismatch"));
    }
    let stored = le_u32(&h[4..8]);
    let mut tmp = h[..4096].to_vec();
    tmp[4..8].fill(0);
    if crc32c(&tmp) != stored {
        return Err(invalid_data("VHDX header CRC mismatch"));
    }
    let version = u16::from_le_bytes([h[66], h[67]]);
    if version != 1 {
        return Err(invalid_data("unsupported VHDX header version"));
    }
    // Log GUID must be all-zero (no log to replay).
    if h[48..64].iter().any(|&b| b != 0) {
        return Err(invalid_data("VHDX has a non-empty log (dirty image)"));
    }
    Ok(VhdxHeader {
        sequence: le_u64(&h[8..16]),
    })
}

/// Choose the valid header with the greatest sequence number.
pub(crate) fn parse_best_header(h1: &[u8], h2: &[u8]) -> io::Result<VhdxHeader> {
    let a = parse_one_header(h1).ok();
    let b = parse_one_header(h2).ok();
    match (a, b) {
        (Some(a), Some(b)) => Ok(if a.sequence >= b.sequence { a } else { b }),
        (Some(a), None) => Ok(a),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(invalid_data("no valid VHDX header")),
    }
}

/// Located regions: each is `(file_offset, length)`.
pub(crate) struct Regions {
    pub bat: Option<(u64, u64)>,
    pub metadata: Option<(u64, u64)>,
}

/// Parse a 64 KiB region table, validating its CRC, and pick out the BAT and
/// metadata regions by GUID.
pub(crate) fn parse_region_table(rt: &[u8]) -> io::Result<Regions> {
    if rt.len() < 64 * 1024 || &rt[0..4] != b"regi" {
        return Err(invalid_data("VHDX region table signature mismatch"));
    }
    let stored = le_u32(&rt[4..8]);
    let mut tmp = rt[..64 * 1024].to_vec();
    tmp[4..8].fill(0);
    if crc32c(&tmp) != stored {
        return Err(invalid_data("VHDX region table CRC mismatch"));
    }
    let count = le_u32(&rt[8..12]) as usize;
    // Each entry is 32 bytes; entries start at offset 16. Bound the count.
    if 16 + count * 32 > 64 * 1024 {
        return Err(invalid_data("VHDX region table entry count too large"));
    }

    let mut regions = Regions {
        bat: None,
        metadata: None,
    };
    for i in 0..count {
        let e = 16 + i * 32;
        let guid = &rt[e..e + 16];
        let off = le_u64(&rt[e + 16..e + 24]);
        let len = le_u32(&rt[e + 24..e + 28]) as u64;
        if guid == BAT_GUID {
            regions.bat = Some((off, len));
        } else if guid == METADATA_GUID {
            regions.metadata = Some((off, len));
        }
    }
    Ok(regions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhdx::fixtures;

    #[test]
    fn picks_header_with_higher_sequence() {
        let img = fixtures::dynamic_vhdx(&[1u8; 1024 * 1024]);
        let h1 = &img[64 * 1024..64 * 1024 + 4096];
        let h2 = &img[128 * 1024..128 * 1024 + 4096];
        let chosen = parse_best_header(h1, h2).unwrap();
        assert_eq!(chosen.sequence, 2);
    }

    #[test]
    fn rejects_header_with_bad_crc() {
        let img = fixtures::dynamic_vhdx(&[1u8; 1024 * 1024]);
        let mut h = img[64 * 1024..64 * 1024 + 4096].to_vec();
        h[100] ^= 0xFF;
        // Both copies corrupted -> no valid header.
        assert!(parse_best_header(&h, &h).is_err());
    }

    #[test]
    fn finds_bat_and_metadata_regions() {
        let img = fixtures::dynamic_vhdx(&[1u8; 1024 * 1024]);
        let rt = &img[192 * 1024..192 * 1024 + 64 * 1024];
        let regions = parse_region_table(rt).unwrap();
        assert!(regions.bat.is_some());
        assert!(regions.metadata.is_some());
        let (off, _len) = regions.metadata.unwrap();
        assert_eq!(off, 1024 * 1024);
    }
}
