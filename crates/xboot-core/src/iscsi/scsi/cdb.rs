//! SCSI CDB opcodes, field parsers, and per-command handlers.
#![allow(dead_code)] // handlers wired in by later 05b tasks

/// SCSI operation codes we recognize (CDB byte 0).
pub mod op {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const INQUIRY: u8 = 0x12;
    pub const MODE_SENSE_6: u8 = 0x1a;
    pub const START_STOP_UNIT: u8 = 0x1b;
    pub const PREVENT_ALLOW: u8 = 0x1e;
    pub const READ_CAPACITY_10: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2a;
    pub const SYNC_CACHE_10: u8 = 0x35;
    pub const MODE_SENSE_10: u8 = 0x5a;
    pub const READ_16: u8 = 0x88;
    pub const WRITE_16: u8 = 0x8a;
    pub const SYNC_CACHE_16: u8 = 0x91;
    pub const SERVICE_ACTION_IN_16: u8 = 0x9e;
    pub const REPORT_LUNS: u8 = 0xa0;
}

/// Service action (CDB byte 1, low 5 bits) for READ CAPACITY(16) under 0x9e.
pub const SAI_READ_CAPACITY_16: u8 = 0x10;

// Big-endian field readers. CDB is always 16 bytes, so offsets are in range.
fn be16(b: &[u8; 16], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}
fn be32(b: &[u8; 16], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn be64(b: &[u8; 16], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// LBA of a 10-byte command (READ(10)/WRITE(10)).
fn lba32(cdb: &[u8; 16]) -> u64 {
    be32(cdb, 2) as u64
}
/// Transfer length (blocks) of a 10-byte command.
fn len16(cdb: &[u8; 16]) -> u64 {
    be16(cdb, 7) as u64
}
/// LBA of a 16-byte command (READ(16)/WRITE(16)).
fn lba64(cdb: &[u8; 16]) -> u64 {
    be64(cdb, 2)
}
/// Transfer length (blocks) of a 16-byte command.
fn len32_16(cdb: &[u8; 16]) -> u64 {
    be32(cdb, 10) as u64
}

use super::sense;
use super::{LogicalUnit, ScsiOutcome};
use crate::iscsi::ScsiCommand;

/// Route a command to its handler. LUN is already resolved; REPORT LUNS is
/// handled by the target before this is called.
pub(super) fn dispatch(lu: &LogicalUnit, cmd: &ScsiCommand, write_data: &[u8]) -> ScsiOutcome {
    let cdb = &cmd.cdb;
    match cdb[0] {
        op::TEST_UNIT_READY => ScsiOutcome::ok(),
        op::INQUIRY => inquiry(lu, cdb),
        op::READ_CAPACITY_10 => read_capacity_10(lu),
        op::SERVICE_ACTION_IN_16 if cdb[1] & 0x1f == SAI_READ_CAPACITY_16 => read_capacity_16(lu),
        op::READ_10 => read(lu, lba32(cdb), len16(cdb)),
        op::READ_16 => read(lu, lba64(cdb), len32_16(cdb)),
        op::WRITE_10 => write(lu, lba32(cdb), len16(cdb), write_data),
        op::WRITE_16 => write(lu, lba64(cdb), len32_16(cdb), write_data),
        op::SYNC_CACHE_10 | op::SYNC_CACHE_16 => ScsiOutcome::ok(),
        op::PREVENT_ALLOW | op::START_STOP_UNIT => ScsiOutcome::ok(),
        op::REQUEST_SENSE => request_sense(),
        op::MODE_SENSE_6 | op::MODE_SENSE_10 => mode_sense(cdb),
        _ => ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_INVALID_OPCODE),
    }
}

fn request_sense() -> ScsiOutcome {
    // No retained contingent allegiance (iSCSI delivers sense inline via the
    // SCSI Response), so report NO SENSE in the data-in buffer with GOOD status.
    ScsiOutcome::good(sense::fixed_sense(sense::NO_SENSE, 0x00, 0x00))
}

fn mode_sense(cdb: &[u8; 16]) -> ScsiOutcome {
    let ten = cdb[0] == op::MODE_SENSE_10;
    let page_code = cdb[2] & 0x3f;
    let mut pages = Vec::new();
    if page_code == 0x08 || page_code == 0x3f {
        pages.extend_from_slice(&caching_page());
    }
    let data = if ten {
        let mut d = vec![0u8; 8]; // 8-byte header, no block descriptors
        let mode_len = (6 + pages.len()) as u16; // total - 2
        d[0..2].copy_from_slice(&mode_len.to_be_bytes());
        d.extend_from_slice(&pages);
        d
    } else {
        let mut d = vec![0u8; 4]; // 4-byte header, no block descriptors
        d[0] = (3 + pages.len()) as u8; // mode data length = total - 1
        d.extend_from_slice(&pages);
        d
    };
    ScsiOutcome::good(data)
}

/// Minimal caching mode page (0x08): 20 bytes, WCE = 1 (write-back enabled).
fn caching_page() -> Vec<u8> {
    let mut p = vec![0u8; 20];
    p[0] = 0x08; // page code
    p[1] = 0x12; // page length (18)
    p[2] = 0x04; // WCE = 1
    p
}

fn write(lu: &LogicalUnit, lba: u64, blocks: u64, write_data: &[u8]) -> ScsiOutcome {
    if blocks == 0 {
        return ScsiOutcome::ok();
    }
    let bs = lu.block_size as u64;
    let in_range = lba
        .checked_add(blocks)
        .map(|end| end <= lu.total_blocks())
        .unwrap_or(false);
    if !in_range {
        return ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_LBA_OUT_OF_RANGE);
    }
    if write_data.len() as u64 != blocks * bs {
        return ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_INVALID_FIELD_IN_CDB);
    }
    match lu.volume.write_at(lba * bs, write_data) {
        Ok(()) => ScsiOutcome::ok(),
        Err(_) => ScsiOutcome::check(sense::MEDIUM_ERROR, sense::ASC_WRITE_ERROR),
    }
}

fn read(lu: &LogicalUnit, lba: u64, blocks: u64) -> ScsiOutcome {
    if blocks == 0 {
        return ScsiOutcome::ok();
    }
    let bs = lu.block_size as u64;
    let in_range = lba
        .checked_add(blocks)
        .map(|end| end <= lu.total_blocks())
        .unwrap_or(false);
    if !in_range {
        return ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_LBA_OUT_OF_RANGE);
    }
    let mut data = vec![0u8; (blocks * bs) as usize];
    match lu.volume.read_at(lba * bs, &mut data) {
        Ok(()) => ScsiOutcome::good(data),
        Err(_) => ScsiOutcome::check(sense::MEDIUM_ERROR, sense::ASC_UNRECOVERED_READ_ERROR),
    }
}

fn read_capacity_10(lu: &LogicalUnit) -> ScsiOutcome {
    let last = lu.total_blocks().saturating_sub(1);
    // If the disk has more than 2^32 blocks, report 0xFFFFFFFF so the
    // initiator falls back to READ CAPACITY(16).
    let returned = if last > u32::MAX as u64 {
        u32::MAX
    } else {
        last as u32
    };
    let mut d = vec![0u8; 8];
    d[0..4].copy_from_slice(&returned.to_be_bytes());
    d[4..8].copy_from_slice(&lu.block_size.to_be_bytes());
    ScsiOutcome::good(d)
}

fn read_capacity_16(lu: &LogicalUnit) -> ScsiOutcome {
    let last = lu.total_blocks().saturating_sub(1);
    let mut d = vec![0u8; 32];
    d[0..8].copy_from_slice(&last.to_be_bytes());
    d[8..12].copy_from_slice(&lu.block_size.to_be_bytes());
    ScsiOutcome::good(d)
}

fn inquiry(lu: &LogicalUnit, cdb: &[u8; 16]) -> ScsiOutcome {
    let evpd = cdb[1] & 0x01 != 0;
    let page = cdb[2];
    if !evpd {
        // Standard INQUIRY: page code must be 0 when EVPD is clear.
        if page != 0 {
            return ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_INVALID_FIELD_IN_CDB);
        }
        return ScsiOutcome::good(lu.standard_inquiry());
    }
    match page {
        0x00 => ScsiOutcome::good(vpd_supported_pages()),
        0x80 => ScsiOutcome::good(lu.vpd_unit_serial()),
        0x83 => ScsiOutcome::good(lu.vpd_device_id()),
        _ => ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_INVALID_FIELD_IN_CDB),
    }
}

/// VPD page 0x00: list of supported VPD pages.
fn vpd_supported_pages() -> Vec<u8> {
    // header(4) + page list; byte3 = number of pages.
    vec![0x00, 0x00, 0x00, 0x03, 0x00, 0x80, 0x83]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read10_lba_and_length_are_big_endian() {
        // READ(10): opcode 0x28, LBA at bytes 2..6, transfer length at 7..9.
        let mut cdb = [0u8; 16];
        cdb[0] = op::READ_10;
        cdb[2..6].copy_from_slice(&0x0001_0002u32.to_be_bytes());
        cdb[7..9].copy_from_slice(&8u16.to_be_bytes());
        assert_eq!(lba32(&cdb), 0x0001_0002);
        assert_eq!(len16(&cdb), 8);
    }

    #[test]
    fn read16_lba_and_length_are_big_endian() {
        // READ(16): LBA at 2..10 (8 bytes), transfer length at 10..14 (4 bytes).
        let mut cdb = [0u8; 16];
        cdb[0] = op::READ_16;
        cdb[2..10].copy_from_slice(&0x0000_0000_DEAD_BEEFu64.to_be_bytes());
        cdb[10..14].copy_from_slice(&4096u32.to_be_bytes());
        assert_eq!(lba64(&cdb), 0xDEAD_BEEF);
        assert_eq!(len32_16(&cdb), 4096);
    }
}
