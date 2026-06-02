//! Build a minimal valid dynamic VHDX from a raw blob, for tests.
//!
//! Layout (1 MiB region alignment, 1 MiB payload blocks):
//!   0       File Identifier ("vhdxfile")
//!   64 KiB  Header 1 (sequence 2, log GUID = 0)
//!   128 KiB Header 2 (sequence 1)
//!   192 KiB Region Table (BAT + Metadata entries)
//!   256 KiB Region Table copy
//!   1 MiB   Metadata region (1 MiB)
//!   2 MiB   BAT region (1 MiB)
//!   3 MiB.. payload blocks, one per MiB

use super::crc32c::crc32c;

const KIB: usize = 1024;
const MIB: u64 = 1024 * 1024;
pub(crate) const BLOCK_SIZE: u64 = MIB;
const LOGICAL_SECTOR: u32 = 512;

// Region GUIDs (raw 16-byte, Microsoft mixed-endian).
const BAT_GUID: [u8; 16] = [
    0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08,
];
const METADATA_GUID: [u8; 16] = [
    0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E,
];
// Metadata item GUIDs.
const FILE_PARAMS_GUID: [u8; 16] = [
    0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B,
];
const VDISK_SIZE_GUID: [u8; 16] = [
    0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8,
];
const LOGICAL_SECTOR_GUID: [u8; 16] = [
    0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F,
];

const METADATA_REGION_OFF: u64 = MIB;
const BAT_REGION_OFF: u64 = 2 * MIB;
const FIRST_BLOCK_OFF: u64 = 3 * MIB;

fn write_header(buf: &mut [u8], sequence: u64) {
    // 4 KiB header at buf[..4096].
    let h = &mut buf[..4096];
    h[0..4].copy_from_slice(b"head");
    h[8..16].copy_from_slice(&sequence.to_le_bytes());
    // log guid (48..64) stays zero -> no log to replay.
    h[66..68].copy_from_slice(&1u16.to_le_bytes()); // version
    let crc = crc32c(h); // checksum field (4..8) currently zero
    h[4..8].copy_from_slice(&crc.to_le_bytes());
}

fn write_region_table(buf: &mut [u8], bat_len: u64) {
    // 64 KiB region table at buf[..65536].
    let r = &mut buf[..64 * KIB];
    r[0..4].copy_from_slice(b"regi");
    r[8..12].copy_from_slice(&2u32.to_le_bytes()); // entry count
                                                   // Entry 0: BAT.
    let e0 = 16;
    r[e0..e0 + 16].copy_from_slice(&BAT_GUID);
    r[e0 + 16..e0 + 24].copy_from_slice(&BAT_REGION_OFF.to_le_bytes());
    r[e0 + 24..e0 + 28].copy_from_slice(&(bat_len as u32).to_le_bytes());
    r[e0 + 28..e0 + 32].copy_from_slice(&1u32.to_le_bytes()); // required
                                                              // Entry 1: Metadata.
    let e1 = 16 + 32;
    r[e1..e1 + 16].copy_from_slice(&METADATA_GUID);
    r[e1 + 16..e1 + 24].copy_from_slice(&METADATA_REGION_OFF.to_le_bytes());
    r[e1 + 24..e1 + 28].copy_from_slice(&(MIB as u32).to_le_bytes());
    r[e1 + 28..e1 + 32].copy_from_slice(&1u32.to_le_bytes());
    let crc = crc32c(r);
    r[4..8].copy_from_slice(&crc.to_le_bytes());
}

fn write_metadata(buf: &mut [u8], virtual_size: u64) {
    // Metadata region (1 MiB) at buf[..MIB].
    let m = &mut buf[..MIB as usize];
    m[0..8].copy_from_slice(b"metadata");
    m[10..12].copy_from_slice(&3u16.to_le_bytes()); // entry count

    // Item values live at fixed offsets within the region.
    let fp_off: u32 = 0x1_0000; // 64 KiB
    let vs_off: u32 = 0x1_0010;
    let ls_off: u32 = 0x1_0020;

    let mut put_entry = |slot: usize, guid: &[u8; 16], off: u32, len: u32| {
        let e = 32 + slot * 32; // entries start at offset 32
        m[e..e + 16].copy_from_slice(guid);
        m[e + 16..e + 20].copy_from_slice(&off.to_le_bytes());
        m[e + 20..e + 24].copy_from_slice(&len.to_le_bytes());
        m[e + 24..e + 28].copy_from_slice(&0u32.to_le_bytes()); // flags
    };
    put_entry(0, &FILE_PARAMS_GUID, fp_off, 8);
    put_entry(1, &VDISK_SIZE_GUID, vs_off, 8);
    put_entry(2, &LOGICAL_SECTOR_GUID, ls_off, 4);

    // File Parameters: block size (4) + flags (4) [bit1 HasParent = 0].
    m[fp_off as usize..fp_off as usize + 4].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    m[fp_off as usize + 4..fp_off as usize + 8].copy_from_slice(&0u32.to_le_bytes());
    // Virtual disk size (8).
    m[vs_off as usize..vs_off as usize + 8].copy_from_slice(&virtual_size.to_le_bytes());
    // Logical sector size (4).
    m[ls_off as usize..ls_off as usize + 4].copy_from_slice(&LOGICAL_SECTOR.to_le_bytes());
}

/// Build a dynamic VHDX whose virtual disk equals `data` (a multiple of 1 MiB).
/// All-zero blocks are left not-present (BAT state 0) to exercise the zero path.
pub(crate) fn dynamic_vhdx(data: &[u8]) -> Vec<u8> {
    assert!(
        (data.len() as u64).is_multiple_of(BLOCK_SIZE),
        "data must be MiB-aligned"
    );
    let virtual_size = data.len() as u64;
    let n_blocks = virtual_size / BLOCK_SIZE;

    // chunk_ratio for 512-byte sectors and 1 MiB blocks = 4096; with few blocks
    // every PB index < chunk_ratio, so no SB entries interleave.
    let bat_entries = n_blocks; // PB entries only (n_blocks < 4096 in tests)
    let bat_len = MIB; // we reserve a full 1 MiB region

    // Assemble the file up to the first payload block (3 MiB), then append blocks.
    let mut out = vec![0u8; FIRST_BLOCK_OFF as usize];

    out[0..8].copy_from_slice(b"vhdxfile"); // File Identifier

    write_header(&mut out[64 * KIB..], 2); // Header 1, higher sequence
    write_header(&mut out[128 * KIB..], 1); // Header 2
    write_region_table(&mut out[192 * KIB..], bat_len);
    write_region_table(&mut out[256 * KIB..], bat_len);
    write_metadata(&mut out[METADATA_REGION_OFF as usize..], virtual_size);

    // Build BAT and append present blocks at 3 MiB, 4 MiB, ...
    let mut next_off = FIRST_BLOCK_OFF;
    let mut bat = vec![0u64; bat_entries as usize];
    for (i, chunk) in data.chunks(BLOCK_SIZE as usize).enumerate() {
        if chunk.iter().all(|&b| b == 0) {
            continue; // state 0 = not present -> zeros
        }
        bat[i] = (next_off & !0xFFFFF) | 6; // FULLY_PRESENT
        out.extend_from_slice(chunk);
        next_off += BLOCK_SIZE;
    }
    // Write the BAT entries into the reserved BAT region.
    let bat_start = BAT_REGION_OFF as usize;
    for (i, e) in bat.iter().enumerate() {
        out[bat_start + i * 8..bat_start + i * 8 + 8].copy_from_slice(&e.to_le_bytes());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_identifier_and_header_signatures() {
        let v = dynamic_vhdx(&[1u8; BLOCK_SIZE as usize]);
        assert_eq!(&v[0..8], b"vhdxfile");
        assert_eq!(&v[64 * KIB..64 * KIB + 4], b"head");
        assert_eq!(&v[192 * KIB..192 * KIB + 4], b"regi");
    }
}
