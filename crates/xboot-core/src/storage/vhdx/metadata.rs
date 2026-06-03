use std::io;

use crate::storage::invalid_data;

const FILE_PARAMS_GUID: [u8; 16] = [
    0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B,
];
const VDISK_SIZE_GUID: [u8; 16] = [
    0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8,
];
const LOGICAL_SECTOR_GUID: [u8; 16] = [
    0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F,
];

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

pub(crate) struct VhdxMetadata {
    pub block_size: u64,
    pub logical_sector_size: u64,
    pub virtual_size: u64,
}

/// Parse the metadata region: its table header plus the three items we need.
/// Rejects images with a parent (differencing VHDX) — masters are standalone.
pub(crate) fn parse_metadata(region: &[u8]) -> io::Result<VhdxMetadata> {
    if region.len() < 32 || &region[0..8] != b"metadata" {
        return Err(invalid_data("VHDX metadata signature mismatch"));
    }
    let count = le_u16(&region[10..12]) as usize;
    if 32 + count * 32 > region.len() {
        return Err(invalid_data("VHDX metadata entry count too large"));
    }

    let mut block_size: Option<u64> = None;
    let mut logical_sector_size: Option<u64> = None;
    let mut virtual_size: Option<u64> = None;

    for i in 0..count {
        let e = 32 + i * 32;
        let guid = &region[e..e + 16];
        let off = le_u32(&region[e + 16..e + 20]) as usize;
        let len = le_u32(&region[e + 20..e + 24]) as usize;
        let end = off
            .checked_add(len)
            .filter(|&end| end <= region.len())
            .ok_or_else(|| invalid_data("VHDX metadata item out of bounds"))?;
        let value = &region[off..end];

        if guid == FILE_PARAMS_GUID {
            if value.len() < 8 {
                return Err(invalid_data("VHDX file parameters too short"));
            }
            block_size = Some(le_u32(&value[0..4]) as u64);
            let flags = le_u32(&value[4..8]);
            if flags & 0b10 != 0 {
                return Err(invalid_data("differencing VHDX (has parent) unsupported"));
            }
        } else if guid == VDISK_SIZE_GUID {
            if value.len() < 8 {
                return Err(invalid_data("VHDX virtual disk size too short"));
            }
            virtual_size = Some(le_u64(&value[0..8]));
        } else if guid == LOGICAL_SECTOR_GUID {
            if value.len() < 4 {
                return Err(invalid_data("VHDX logical sector size too short"));
            }
            logical_sector_size = Some(le_u32(&value[0..4]) as u64);
        }
    }

    let block_size = block_size.ok_or_else(|| invalid_data("VHDX missing block size"))?;
    let logical_sector_size =
        logical_sector_size.ok_or_else(|| invalid_data("VHDX missing logical sector size"))?;
    let virtual_size = virtual_size.ok_or_else(|| invalid_data("VHDX missing virtual size"))?;

    if block_size == 0 || !block_size.is_power_of_two() {
        return Err(invalid_data("invalid VHDX block size"));
    }
    if logical_sector_size == 0 || !logical_sector_size.is_power_of_two() {
        return Err(invalid_data("invalid VHDX logical sector size"));
    }
    Ok(VhdxMetadata {
        block_size,
        logical_sector_size,
        virtual_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhdx::fixtures;

    #[test]
    fn reads_block_size_sector_and_virtual_size() {
        let img = fixtures::dynamic_vhdx(&[1u8; 2 * 1024 * 1024]);
        let region = &img[1024 * 1024..2 * 1024 * 1024]; // metadata region (1 MiB)
        let md = parse_metadata(region).unwrap();
        assert_eq!(md.block_size, 1024 * 1024);
        assert_eq!(md.logical_sector_size, 512);
        assert_eq!(md.virtual_size, 2 * 1024 * 1024);
    }

    #[test]
    fn rejects_bad_signature() {
        let mut region = vec![0u8; 1024 * 1024];
        region[0..8].copy_from_slice(b"NOTMETA!");
        assert!(parse_metadata(&region).is_err());
    }
}
