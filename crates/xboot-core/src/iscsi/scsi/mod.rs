//! SCSI command execution (phase 05b).
//!
//! Pure, synchronous bridge from a decoded `ScsiCommand` (CDB + LUN) to a
//! `ScsiOutcome` (status + data + sense), executed against a per-LUN `Volume`.
//! No wire framing, no sequence numbers, no Data-In chunking — that is 05c.
#![allow(dead_code)] // fields and helpers wired in by later 05b tasks

mod cdb;
mod sense;

use crate::iscsi::ScsiCommand;
use crate::volume::Volume;

/// Result of executing one SCSI command. 05c serializes this onto the wire
/// (Data-In PDUs for `data`, SCSI Response carrying `status` and `sense`).
pub struct ScsiOutcome {
    pub status: u8,
    pub data: Vec<u8>,
    pub sense: Vec<u8>,
}

impl ScsiOutcome {
    /// GOOD status, no data.
    pub(crate) fn ok() -> Self {
        Self { status: sense::GOOD, data: Vec::new(), sense: Vec::new() }
    }
    /// GOOD status carrying a data-in payload.
    pub(crate) fn good(data: Vec<u8>) -> Self {
        Self { status: sense::GOOD, data, sense: Vec::new() }
    }
    /// CHECK CONDITION with fixed-format sense.
    pub(crate) fn check(key: u8, (asc, ascq): (u8, u8)) -> Self {
        Self {
            status: sense::CHECK_CONDITION,
            data: Vec::new(),
            sense: sense::fixed_sense(key, asc, ascq),
        }
    }
}

/// One LUN: a virtual disk plus the identity it reports to the initiator.
pub struct LogicalUnit {
    pub(crate) volume: Volume,
    pub(crate) block_size: u32,
    pub(crate) vendor: [u8; 8],
    pub(crate) product: [u8; 16],
    pub(crate) revision: [u8; 4],
    pub(crate) serial: String,
    pub(crate) removable: bool,
}

impl LogicalUnit {
    /// A direct-access block device with XBoot defaults (512-byte blocks).
    pub fn new(volume: Volume) -> Self {
        Self {
            volume,
            block_size: 512,
            vendor: *b"XBOOT   ",
            product: *b"VDISK           ",
            revision: *b"0001",
            serial: String::from("XBOOT0000000001"),
            removable: false,
        }
    }
    pub fn with_block_size(mut self, bs: u32) -> Self {
        self.block_size = bs;
        self
    }
    pub fn with_serial(mut self, s: impl Into<String>) -> Self {
        self.serial = s.into();
        self
    }
    /// Number of addressable logical blocks (floor; a short tail is not addressable).
    pub(crate) fn total_blocks(&self) -> u64 {
        self.volume.size_bytes() / self.block_size as u64
    }

    /// Standard INQUIRY data (36 bytes): direct-access block device, SPC-3.
    pub(crate) fn standard_inquiry(&self) -> Vec<u8> {
        let mut d = vec![0u8; 36];
        d[0] = 0x00; // peripheral qualifier 000b + device type 0x00 (block device)
        d[1] = if self.removable { 0x80 } else { 0x00 };
        d[2] = 0x05; // SPC-3
        d[3] = 0x02; // response data format
        d[4] = 31; // additional length (36 - 5)
        d[8..16].copy_from_slice(&self.vendor);
        d[16..32].copy_from_slice(&self.product);
        d[32..36].copy_from_slice(&self.revision);
        d
    }

    /// VPD page 0x80 (unit serial number).
    pub(crate) fn vpd_unit_serial(&self) -> Vec<u8> {
        let sn = self.serial.as_bytes();
        let mut d = vec![0u8; 4 + sn.len()];
        d[0] = 0x00; // device type
        d[1] = 0x80; // page code
        d[3] = sn.len() as u8; // page length
        d[4..].copy_from_slice(sn);
        d
    }

    /// VPD page 0x83 (device identification): one T10 vendor-ID designator.
    pub(crate) fn vpd_device_id(&self) -> Vec<u8> {
        let mut id = Vec::new();
        id.extend_from_slice(&self.vendor);
        id.extend_from_slice(&self.product);
        id.extend_from_slice(self.serial.as_bytes());
        let mut desc = vec![0u8; 4];
        desc[0] = 0x02; // code set: ASCII
        desc[1] = 0x01; // designator type 1 (T10 vendor ID), association 00
        desc[3] = id.len() as u8; // designator length
        desc.extend_from_slice(&id);
        let mut d = vec![0u8; 4];
        d[0] = 0x00; // device type
        d[1] = 0x83; // page code
        let plen = desc.len() as u16;
        d[2..4].copy_from_slice(&plen.to_be_bytes());
        d.extend_from_slice(&desc);
        d
    }
}

/// One iSCSI target = one client. Owns its LUNs, indexed by LUN number.
pub struct ScsiTarget {
    luns: Vec<Option<LogicalUnit>>,
}

impl ScsiTarget {
    pub fn new(luns: Vec<Option<LogicalUnit>>) -> Self {
        Self { luns }
    }

    /// Execute one SCSI command. `write_data` is the fully-assembled write
    /// payload (05c gathers it from immediate data / Data-Out); ignored for
    /// non-write commands.
    pub fn execute(&self, cmd: &ScsiCommand, write_data: &[u8]) -> ScsiOutcome {
        let lu = match lun_number(cmd.lun).and_then(|n| self.luns.get(n).and_then(|o| o.as_ref())) {
            Some(lu) => lu,
            None => return ScsiOutcome::check(sense::ILLEGAL_REQUEST, sense::ASC_LUN_NOT_SUPPORTED),
        };
        if cmd.cdb[0] == cdb::op::REPORT_LUNS {
            return self.report_luns();
        }
        cdb::dispatch(lu, cmd, write_data)
    }

    /// REPORT LUNS sees the whole map, so it lives on the target, not a LU.
    fn report_luns(&self) -> ScsiOutcome {
        let present: Vec<usize> = self
            .luns
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.as_ref().map(|_| i))
            .collect();
        let list_len = (present.len() * 8) as u32;
        let mut d = vec![0u8; 8];
        d[0..4].copy_from_slice(&list_len.to_be_bytes());
        for n in present {
            let mut lun = [0u8; 8];
            lun[1] = n as u8; // single-level peripheral device addressing
            d.extend_from_slice(&lun);
        }
        ScsiOutcome::good(d)
    }
}

/// Decode the LUN number from the 8-byte iSCSI LUN field, single-level
/// peripheral device addressing. Returns `None` for any other address method.
fn lun_number(lun: u64) -> Option<usize> {
    if lun >> 56 != 0 {
        return None; // address method / bus must be 0 for our flat addressing
    }
    Some(((lun >> 48) & 0xff) as usize)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::storage::BackingStore;
    use crate::volume::RamOverlay;
    use std::io;

    /// In-memory read-only master for tests.
    pub(crate) struct MemStore(pub Vec<u8>);
    impl BackingStore for MemStore {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let start = offset as usize;
            buf.copy_from_slice(&self.0[start..start + buf.len()]);
            Ok(())
        }
    }

    /// A `LogicalUnit` over an in-memory master of `bytes`, 512-byte blocks.
    pub(crate) fn lu(bytes: Vec<u8>) -> LogicalUnit {
        let vol = Volume::new(Box::new(MemStore(bytes)), Box::new(RamOverlay::new()));
        LogicalUnit::new(vol)
    }

    /// A single-LUN target (LUN 0) over an in-memory master of `bytes`.
    pub(crate) fn target(bytes: Vec<u8>) -> ScsiTarget {
        ScsiTarget::new(vec![Some(lu(bytes))])
    }

    /// Build a `ScsiCommand` for LUN `lun` with the given CDB.
    pub(crate) fn cmd(lun: u64, cdb: [u8; 16]) -> ScsiCommand {
        ScsiCommand {
            final_: true,
            read: false,
            write: false,
            attr: 0,
            lun,
            itt: 1,
            edtl: 0,
            cmd_sn: 0,
            exp_stat_sn: 0,
            cdb,
            data: Vec::new(),
        }
    }

    /// Encode LUN number `n` into the 8-byte iSCSI LUN field (flat addressing).
    pub(crate) fn lun_field(n: u8) -> u64 {
        (n as u64) << 48
    }
}

#[cfg(test)]
mod tests {
    use super::cdb::op;
    use super::sense;
    use super::test_support::*;
    use super::ScsiTarget;

    #[test]
    fn lun_number_decodes_flat_addressing() {
        assert_eq!(super::lun_number(lun_field(0)), Some(0));
        assert_eq!(super::lun_number(lun_field(1)), Some(1));
        // High address-method byte set -> unsupported.
        assert_eq!(super::lun_number(1u64 << 56), None);
    }

    #[test]
    fn test_unit_ready_is_good() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::TEST_UNIT_READY;
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert!(out.sense.is_empty());
    }

    #[test]
    fn unknown_opcode_is_check_condition_invalid_opcode() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = 0x3c; // not a recognized opcode
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::CHECK_CONDITION);
        assert_eq!(out.sense[2] & 0x0f, sense::ILLEGAL_REQUEST);
        assert_eq!(out.sense[12], sense::ASC_INVALID_OPCODE.0);
    }

    #[test]
    fn unknown_lun_is_check_condition() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::TEST_UNIT_READY;
        let out = t.execute(&cmd(lun_field(7), cdb), &[]); // LUN 7 not configured
        assert_eq!(out.status, sense::CHECK_CONDITION);
        assert_eq!(out.sense[12], sense::ASC_LUN_NOT_SUPPORTED.0);
    }

    #[test]
    fn standard_inquiry_reports_block_device_and_idents() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::INQUIRY;
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(out.data[0], 0x00); // direct-access block device
        assert_eq!(out.data[2], 0x05); // SPC-3
        assert_eq!(out.data[4], 31); // additional length
        assert_eq!(&out.data[8..16], b"XBOOT   ");
        assert_eq!(&out.data[16..21], b"VDISK");
    }

    #[test]
    fn inquiry_evpd_serial_page_carries_serial() {
        let t = ScsiTarget::new(vec![Some(lu(vec![0u8; 4096]).with_serial("ABC123"))]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::INQUIRY;
        cdb[1] = 0x01; // EVPD
        cdb[2] = 0x80; // unit serial number page
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(out.data[1], 0x80); // page code echoed
        assert_eq!(&out.data[4..], b"ABC123");
    }

    #[test]
    fn inquiry_evpd_supported_pages_lists_the_three() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::INQUIRY;
        cdb[1] = 0x01;
        cdb[2] = 0x00; // supported VPD pages
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(&out.data[4..7], &[0x00, 0x80, 0x83]);
    }

    #[test]
    fn inquiry_unknown_vpd_page_is_invalid_field() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::INQUIRY;
        cdb[1] = 0x01;
        cdb[2] = 0xde; // not a supported page
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::CHECK_CONDITION);
        assert_eq!(out.data.len(), 0);
        assert_eq!(out.sense[12], sense::ASC_INVALID_FIELD_IN_CDB.0);
    }
}
