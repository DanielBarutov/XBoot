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
        b[off], b[off + 1], b[off + 2], b[off + 3],
        b[off + 4], b[off + 5], b[off + 6], b[off + 7],
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
pub(super) fn dispatch(lu: &LogicalUnit, cmd: &ScsiCommand, _write_data: &[u8]) -> ScsiOutcome {
    let cdb = &cmd.cdb;
    match cdb[0] {
        op::TEST_UNIT_READY => ScsiOutcome::ok(),
        op::INQUIRY => inquiry(lu, cdb),
        _ => ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_INVALID_OPCODE),
    }
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
