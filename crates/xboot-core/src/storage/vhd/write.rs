//! Writer for dynamic VHDs. Used by `xboot flatten` to materialize a
//! `BackingStore` (e.g. a CCBoot increment chain) into one standalone sparse
//! VHD that Windows and our own reader both accept.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use crate::storage::vhd::footer::DISK_TYPE_DYNAMIC;
use crate::storage::BackingStore;

const SECTOR: u64 = 512;
/// 2 MiB blocks — what CCBoot and Hyper-V both use; keeps the BAT small.
const DEFAULT_BLOCK_SIZE: u32 = 2 * 1024 * 1024;

/// Ones-complement checksum over a 512-byte structure with `skip` bytes (the
/// checksum field itself) treated as zero.
fn checksum(buf: &[u8], skip: std::ops::Range<usize>) -> u32 {
    let mut sum: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if skip.contains(&i) {
            continue;
        }
        sum = sum.wrapping_add(b as u32);
    }
    !sum
}

/// VHD CHS geometry from total sector count (per the VHD spec, appendix).
fn geometry(total_sectors: u64) -> (u16, u8, u8) {
    let ts = total_sectors.min(65535 * 16 * 255);
    let (mut spt, mut heads, mut cth);
    if ts >= 65535 * 16 * 63 {
        spt = 255;
        heads = 16;
        cth = ts / spt;
    } else {
        spt = 17;
        cth = ts / spt;
        heads = cth.div_ceil(1024);
        if heads < 4 {
            heads = 4;
        }
        if cth >= heads * 1024 || heads > 16 {
            spt = 31;
            heads = 16;
            cth = ts / spt;
        }
        if cth >= heads * 1024 {
            spt = 63;
            heads = 16;
            cth = ts / spt;
        }
    }
    let cyls = cth / heads;
    (cyls as u16, heads as u8, spt as u8)
}

/// Build a 512-byte dynamic-disk footer.
fn build_footer(current_size: u64) -> [u8; 512] {
    let mut f = [0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    f[8..12].copy_from_slice(&0x0000_0002u32.to_be_bytes()); // features: reserved bit
    f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // file format version
    f[16..24].copy_from_slice(&512u64.to_be_bytes()); // data offset -> dynamic header
                                                      // timestamp: seconds since 2000-01-01.
    let secs_since_2000 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(946_684_800))
        .unwrap_or(0) as u32;
    f[24..28].copy_from_slice(&secs_since_2000.to_be_bytes());
    f[28..32].copy_from_slice(b"xbot"); // creator application
    f[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // creator version
    f[36..40].copy_from_slice(b"Wi2k"); // creator host OS (Windows)
    f[40..48].copy_from_slice(&current_size.to_be_bytes()); // original size
    f[48..56].copy_from_slice(&current_size.to_be_bytes()); // current size
    let (cyl, heads, spt) = geometry(current_size / SECTOR);
    f[56..58].copy_from_slice(&cyl.to_be_bytes());
    f[58] = heads;
    f[59] = spt;
    f[60..64].copy_from_slice(&DISK_TYPE_DYNAMIC.to_be_bytes());
    // unique id (bytes 68..84): derive a stable-ish UUID from size + time.
    let mut uuid = [0u8; 16];
    uuid[0..8].copy_from_slice(&current_size.to_le_bytes());
    uuid[8..12].copy_from_slice(&secs_since_2000.to_le_bytes());
    uuid[12..16].copy_from_slice(b"xbot");
    f[68..84].copy_from_slice(&uuid);
    let cks = checksum(&f, 64..68);
    f[64..68].copy_from_slice(&cks.to_be_bytes());
    f
}

/// Write `source`'s full contents to `out_path` as a dynamic VHD. Blocks whose
/// bytes are entirely zero are left unallocated, keeping the file sparse.
/// `progress(done_bytes, total_bytes)` is called periodically.
pub fn write_dynamic_vhd(
    out_path: &Path,
    source: &dyn BackingStore,
    progress: impl Fn(u64, u64),
) -> io::Result<()> {
    let virtual_size = source.size_bytes();
    let block_size = DEFAULT_BLOCK_SIZE as u64;
    // Round the virtual size up to a whole block for the BAT geometry.
    let n_blocks = virtual_size.div_ceil(block_size);
    if n_blocks > u32::MAX as u64 {
        return Err(crate::storage::invalid_data("image too large for VHD BAT"));
    }
    let sectors_per_block = block_size / SECTOR;
    let bitmap_bytes_raw = sectors_per_block.div_ceil(8);
    let bitmap_size = bitmap_bytes_raw.div_ceil(SECTOR) * SECTOR;

    let table_offset: u64 = 512 + 1024;
    let bat_bytes = n_blocks * 4;
    let bat_padded = bat_bytes.div_ceil(SECTOR) * SECTOR;
    let blocks_start = table_offset + bat_padded;

    let footer = build_footer(virtual_size);

    let mut file = File::create(out_path)?;

    // Footer copy (offset 0).
    file.write_all(&footer)?;

    // Dynamic disk header (1024 bytes).
    let mut hdr = [0u8; 1024];
    hdr[0..8].copy_from_slice(b"cxsparse");
    hdr[8..16].copy_from_slice(&u64::MAX.to_be_bytes()); // data offset (none)
    hdr[16..24].copy_from_slice(&table_offset.to_be_bytes());
    hdr[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // header version
    hdr[28..32].copy_from_slice(&(n_blocks as u32).to_be_bytes()); // max table entries
    hdr[32..36].copy_from_slice(&(block_size as u32).to_be_bytes());
    let hcks = checksum(&hdr, 36..40);
    hdr[36..40].copy_from_slice(&hcks.to_be_bytes());
    file.write_all(&hdr)?;

    // Reserve space for the BAT; we backfill it after writing the blocks.
    file.write_all(&vec![0xFFu8; bat_padded as usize])?;

    let mut bat = vec![0xFFFF_FFFFu32; n_blocks as usize];
    let mut next_block_sector = (blocks_start / SECTOR) as u32;
    let full_bitmap = {
        let mut b = vec![0u8; bitmap_size as usize];
        for x in b.iter_mut().take(bitmap_bytes_raw as usize) {
            *x = 0xFF;
        }
        b
    };

    let mut block_buf = vec![0u8; block_size as usize];
    for i in 0..n_blocks {
        let off = i * block_size;
        let this = block_size.min(virtual_size - off) as usize;
        // Zero the tail when the last block is partial.
        if this < block_buf.len() {
            block_buf[this..].fill(0);
        }
        source.read_at(off, &mut block_buf[..this])?;

        if block_buf.iter().all(|&b| b == 0) {
            continue; // leave unallocated (reads back as zeros)
        }
        bat[i as usize] = next_block_sector;
        file.write_all(&full_bitmap)?;
        file.write_all(&block_buf)?;
        next_block_sector += ((bitmap_size + block_size) / SECTOR) as u32;

        if i % 256 == 0 {
            progress(off, virtual_size);
        }
    }

    // Trailing footer.
    file.write_all(&footer)?;

    // Backfill the BAT now that every entry is known.
    file.seek(SeekFrom::Start(table_offset))?;
    let mut bat_bytes_buf = Vec::with_capacity((n_blocks * 4) as usize);
    for e in &bat {
        bat_bytes_buf.extend_from_slice(&e.to_be_bytes());
    }
    file.write_all(&bat_bytes_buf)?;
    file.flush()?;
    progress(virtual_size, virtual_size);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::open_backing;
    use std::io;

    struct PatternStore {
        size: u64,
    }
    impl BackingStore for PatternStore {
        fn size_bytes(&self) -> u64 {
            self.size
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            // Deterministic: nonzero only in the first and third 2MiB block.
            for (k, b) in buf.iter_mut().enumerate() {
                let pos = offset + k as u64;
                let block = pos / (2 * 1024 * 1024);
                *b = if block == 0 || block == 2 {
                    (pos % 251) as u8
                } else {
                    0
                };
            }
            Ok(())
        }
    }

    #[test]
    fn round_trip_through_dynamic_vhd() {
        let size = 5 * 1024 * 1024 + 4096; // 2.x blocks, unaligned tail
        let src = PatternStore { size };
        let dir = std::env::temp_dir().join(format!("xboot-wr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("flat.vhd");

        write_dynamic_vhd(&out, &src, |_, _| {}).unwrap();

        // Reopen via the normal detection path and compare every byte.
        let store = open_backing(&out).unwrap();
        assert_eq!(store.size_bytes(), size);

        let mut got = vec![0u8; size as usize];
        store.read_at(0, &mut got).unwrap();
        let mut want = vec![0u8; size as usize];
        src.read_at(0, &mut want).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn all_zero_source_allocates_nothing() {
        struct Zero(u64);
        impl BackingStore for Zero {
            fn size_bytes(&self) -> u64 {
                self.0
            }
            fn read_at(&self, _o: u64, buf: &mut [u8]) -> io::Result<()> {
                buf.fill(0);
                Ok(())
            }
        }
        let dir = std::env::temp_dir().join(format!("xboot-wr0-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("z.vhd");
        write_dynamic_vhd(&out, &Zero(4 * 1024 * 1024), |_, _| {}).unwrap();

        // Header(512+1024) + BAT(512) + footer(512) only, no data blocks.
        let len = std::fs::metadata(&out).unwrap().len();
        assert_eq!(len, 512 + 1024 + 512 + 512);

        let store = open_backing(&out).unwrap();
        let mut got = vec![0xAAu8; 4096];
        store.read_at(0, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0));
    }
}
