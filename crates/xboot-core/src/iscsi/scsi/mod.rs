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

    #[test]
    fn read_capacity_10_reports_last_lba_and_block_len() {
        // 4096 bytes / 512 = 8 blocks -> last LBA = 7.
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::READ_CAPACITY_10;
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(out.data.len(), 8);
        assert_eq!(u32::from_be_bytes([out.data[0], out.data[1], out.data[2], out.data[3]]), 7);
        assert_eq!(u32::from_be_bytes([out.data[4], out.data[5], out.data[6], out.data[7]]), 512);
    }

    #[test]
    fn read_capacity_16_reports_8byte_last_lba() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::SERVICE_ACTION_IN_16;
        cdb[1] = 0x10; // READ CAPACITY (16) service action
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(out.data.len(), 32);
        let last = u64::from_be_bytes(out.data[0..8].try_into().unwrap());
        assert_eq!(last, 7);
        assert_eq!(u32::from_be_bytes(out.data[8..12].try_into().unwrap()), 512);
    }

    #[test]
    fn service_action_in_16_other_action_is_invalid_opcode() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::SERVICE_ACTION_IN_16;
        cdb[1] = 0x12; // not READ CAPACITY(16)
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::CHECK_CONDITION);
        assert_eq!(out.sense[12], sense::ASC_INVALID_OPCODE.0);
    }

    #[test]
    fn read10_returns_master_bytes() {
        // Master: 8 blocks of 512; fill block 1 (bytes 512..1024) with 0xAB.
        let mut master = vec![0u8; 4096];
        for b in &mut master[512..1024] {
            *b = 0xAB;
        }
        let t = target(master);
        let mut cdb = [0u8; 16];
        cdb[0] = op::READ_10;
        cdb[2..6].copy_from_slice(&1u32.to_be_bytes()); // LBA 1
        cdb[7..9].copy_from_slice(&1u16.to_be_bytes()); // 1 block
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(out.data.len(), 512);
        assert!(out.data.iter().all(|&x| x == 0xAB));
    }

    #[test]
    fn read10_zero_length_is_good_no_data() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::READ_10;
        // transfer length 0
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert!(out.data.is_empty());
    }

    #[test]
    fn read10_past_end_is_lba_out_of_range() {
        let t = target(vec![0u8; 4096]); // 8 blocks
        let mut cdb = [0u8; 16];
        cdb[0] = op::READ_10;
        cdb[2..6].copy_from_slice(&7u32.to_be_bytes()); // LBA 7
        cdb[7..9].copy_from_slice(&2u16.to_be_bytes()); // 2 blocks -> ends at 9 > 8
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::CHECK_CONDITION);
        assert_eq!(out.sense[12], sense::ASC_LBA_OUT_OF_RANGE.0);
    }

    #[test]
    fn read16_returns_master_bytes() {
        let mut master = vec![0u8; 4096];
        for b in &mut master[0..512] {
            *b = 0xCD;
        }
        let t = target(master);
        let mut cdb = [0u8; 16];
        cdb[0] = op::READ_16;
        // LBA 0 (bytes 2..10 already zero), 1 block at 10..14
        cdb[10..14].copy_from_slice(&1u32.to_be_bytes());
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(out.data.len(), 512);
        assert!(out.data.iter().all(|&x| x == 0xCD));
    }

    #[test]
    fn write10_then_read10_round_trips_through_overlay() {
        let t = target(vec![0u8; 4096]);
        let payload = vec![0x5Au8; 512];
        let mut w = [0u8; 16];
        w[0] = op::WRITE_10;
        w[2..6].copy_from_slice(&2u32.to_be_bytes()); // LBA 2
        w[7..9].copy_from_slice(&1u16.to_be_bytes()); // 1 block
        let out = t.execute(&cmd(lun_field(0), w), &payload);
        assert_eq!(out.status, sense::GOOD);

        let mut r = [0u8; 16];
        r[0] = op::READ_10;
        r[2..6].copy_from_slice(&2u32.to_be_bytes());
        r[7..9].copy_from_slice(&1u16.to_be_bytes());
        let back = t.execute(&cmd(lun_field(0), r), &[]);
        assert_eq!(back.data, payload);
    }

    #[test]
    fn write10_wrong_buffer_len_is_invalid_field() {
        let t = target(vec![0u8; 4096]);
        let mut w = [0u8; 16];
        w[0] = op::WRITE_10;
        w[7..9].copy_from_slice(&1u16.to_be_bytes()); // expects 512 bytes
        let out = t.execute(&cmd(lun_field(0), w), &[0u8; 100]); // wrong length
        assert_eq!(out.status, sense::CHECK_CONDITION);
        assert_eq!(out.sense[12], sense::ASC_INVALID_FIELD_IN_CDB.0);
    }

    #[test]
    fn write10_past_end_is_lba_out_of_range() {
        let t = target(vec![0u8; 4096]); // 8 blocks
        let mut w = [0u8; 16];
        w[0] = op::WRITE_10;
        w[2..6].copy_from_slice(&8u32.to_be_bytes()); // LBA 8 == total -> out of range
        w[7..9].copy_from_slice(&1u16.to_be_bytes());
        let out = t.execute(&cmd(lun_field(0), w), &[0u8; 512]);
        assert_eq!(out.status, sense::CHECK_CONDITION);
        assert_eq!(out.sense[12], sense::ASC_LBA_OUT_OF_RANGE.0);
    }

    #[test]
    fn write16_round_trips() {
        let t = target(vec![0u8; 4096]);
        let payload = vec![0x33u8; 512];
        let mut w = [0u8; 16];
        w[0] = op::WRITE_16;
        // LBA 0, 1 block at 10..14
        w[10..14].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(t.execute(&cmd(lun_field(0), w), &payload).status, sense::GOOD);

        let mut r = [0u8; 16];
        r[0] = op::READ_16;
        r[10..14].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(t.execute(&cmd(lun_field(0), r), &[]).data, payload);
    }

    #[test]
    fn sync_cache_and_medium_and_start_stop_are_good() {
        let t = target(vec![0u8; 4096]);
        for op_code in [op::SYNC_CACHE_10, op::SYNC_CACHE_16, op::PREVENT_ALLOW, op::START_STOP_UNIT] {
            let mut cdb = [0u8; 16];
            cdb[0] = op_code;
            let out = t.execute(&cmd(lun_field(0), cdb), &[]);
            assert_eq!(out.status, sense::GOOD, "opcode {op_code:#x}");
            assert!(out.sense.is_empty());
        }
    }

    #[test]
    fn request_sense_returns_no_sense() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::REQUEST_SENSE;
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD); // sense rides in the data-in buffer
        assert_eq!(out.data.len(), 18);
        assert_eq!(out.data[0], 0x70);
        assert_eq!(out.data[2] & 0x0f, sense::NO_SENSE);
    }

    #[test]
    fn mode_sense_6_returns_header() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::MODE_SENSE_6;
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        assert_eq!(out.data.len(), 4); // bare 4-byte header, no block descriptors
        assert_eq!(out.data[0], 3); // mode data length = len - 1
    }

    #[test]
    fn mode_sense_6_caching_page_sets_wce() {
        let t = target(vec![0u8; 4096]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::MODE_SENSE_6;
        cdb[2] = 0x08; // caching mode page
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        // 4-byte header + 20-byte caching page.
        assert_eq!(out.data.len(), 24);
        assert_eq!(out.data[4], 0x08); // page code
        assert_eq!(out.data[6] & 0x04, 0x04); // WCE
    }

    #[test]
    fn report_luns_lists_configured_luns() {
        // LUN 0 and LUN 2 present, LUN 1 absent.
        let t = ScsiTarget::new(vec![
            Some(lu(vec![0u8; 4096])),
            None,
            Some(lu(vec![0u8; 4096])),
        ]);
        let mut cdb = [0u8; 16];
        cdb[0] = op::REPORT_LUNS;
        let out = t.execute(&cmd(lun_field(0), cdb), &[]);
        assert_eq!(out.status, sense::GOOD);
        // header(8) + 2 LUNs * 8 = 24 bytes.
        assert_eq!(out.data.len(), 24);
        let list_len = u32::from_be_bytes(out.data[0..4].try_into().unwrap());
        assert_eq!(list_len, 16); // 2 LUNs * 8 bytes
        assert_eq!(out.data[9], 0); // first LUN number == 0 (byte 1 of the 8-byte LUN)
        assert_eq!(out.data[17], 2); // second LUN number == 2
    }
}

#[cfg(test)]
mod prop {
    use super::sense;
    use super::test_support::*;
    use proptest::prelude::*;

    proptest! {
        // execute never panics on an arbitrary CDB + arbitrary LUN field.
        #[test]
        fn execute_never_panics(cdb in proptest::array::uniform16(any::<u8>()), lun in any::<u64>()) {
            let t = target(vec![0u8; 4096]);
            let _ = t.execute(&cmd(lun, cdb), &[]);
        }

        // Status/sense invariant: CHECK CONDITION <=> non-empty sense.
        #[test]
        fn status_matches_sense_presence(cdb in proptest::array::uniform16(any::<u8>())) {
            let t = target(vec![0u8; 4096]);
            let out = t.execute(&cmd(lun_field(0), cdb), &[]);
            if out.status == sense::CHECK_CONDITION {
                prop_assert!(!out.sense.is_empty());
            } else {
                prop_assert_eq!(out.status, sense::GOOD);
                prop_assert!(out.sense.is_empty());
            }
        }

        // A READ(10) fully in range returns exactly blocks * block_size bytes.
        #[test]
        fn read10_in_range_data_length(lba in 0u16..8, blocks in 1u16..=8) {
            // 8-block disk; only keep cases that stay in range.
            prop_assume!(lba as u32 + blocks as u32 <= 8);
            let t = target(vec![0u8; 4096]);
            let mut cdb = [0u8; 16];
            cdb[0] = super::cdb::op::READ_10;
            cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
            cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
            let out = t.execute(&cmd(lun_field(0), cdb), &[]);
            prop_assert_eq!(out.status, sense::GOOD);
            prop_assert_eq!(out.data.len(), blocks as usize * 512);
        }
    }
}
