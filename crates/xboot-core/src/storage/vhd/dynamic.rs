use std::fs::File;
use std::io;
use std::path::Path;

use crate::storage::blockmap::{read_units, Unit};
use crate::storage::file_ext::read_exact_at;
use crate::storage::vhd::footer::{VhdFooter, DISK_TYPE_DYNAMIC};
use crate::storage::{invalid_data, BackingStore};

const SECTOR: u64 = 512;

/// Dynamic VHD: footer + dynamic header + BAT + sparse blocks.
pub struct DynamicVhd {
    file: File,
    virtual_size: u64,
    block_size: u64,
    bitmap_size: u64,
    /// One BAT entry per block: sector offset of the block, or `0xFFFFFFFF`.
    bat: Vec<u32>,
    /// Per-block sector bitmaps, preloaded for allocated blocks (None = hole).
    bitmaps: Vec<Option<Box<[u8]>>>,
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl DynamicVhd {
    /// Open a dynamic VHD read-only. Parses the footer, dynamic header, and BAT.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < 512 {
            return Err(invalid_data("VHD file shorter than a footer"));
        }

        let mut footer_buf = [0u8; 512];
        read_exact_at(&file, file_len - 512, &mut footer_buf)?;
        let footer = VhdFooter::parse(&footer_buf)?;
        if footer.disk_type != DISK_TYPE_DYNAMIC {
            return Err(invalid_data("not a dynamic VHD"));
        }
        let virtual_size = footer.current_size;

        // Dynamic disk header at footer.data_offset.
        let mut hdr = [0u8; 1024];
        read_exact_at(&file, footer.data_offset, &mut hdr)?;
        if &hdr[0..8] != b"cxsparse" {
            return Err(invalid_data("dynamic header cookie mismatch"));
        }
        let table_offset = be_u64(&hdr[16..24]);
        let max_entries = be_u32(&hdr[28..32]) as u64;
        let block_size = be_u32(&hdr[32..36]) as u64;
        if block_size == 0 || !block_size.is_multiple_of(SECTOR) || !block_size.is_power_of_two() {
            return Err(invalid_data("invalid dynamic VHD block size"));
        }
        // Guard against absurd BAT sizes from corrupt headers.
        if max_entries > (1u64 << 32) {
            return Err(invalid_data("dynamic VHD BAT too large"));
        }

        let sectors_per_block = block_size / SECTOR;
        let bitmap_bytes_raw = sectors_per_block.div_ceil(8);
        let bitmap_size = bitmap_bytes_raw.div_ceil(SECTOR) * SECTOR;

        // Read the BAT.
        let bat_bytes = (max_entries as usize)
            .checked_mul(4)
            .ok_or_else(|| invalid_data("BAT size overflow"))?;
        let mut raw = vec![0u8; bat_bytes];
        read_exact_at(&file, table_offset, &mut raw)?;
        let bat: Vec<u32> = raw.chunks_exact(4).map(be_u32).collect();

        // Preload sector bitmaps so reads never touch the disk to test a bit.
        let mut bitmaps = Vec::with_capacity(bat.len());
        for &entry in &bat {
            if entry == 0xFFFF_FFFF {
                bitmaps.push(None);
            } else {
                let mut bm = vec![0u8; bitmap_size as usize];
                read_exact_at(&file, entry as u64 * SECTOR, &mut bm)?;
                bitmaps.push(Some(bm.into_boxed_slice()));
            }
        }

        Ok(Self {
            file,
            virtual_size,
            block_size,
            bitmap_size,
            bat,
            bitmaps,
        })
    }

    /// Physical byte offset of `sector`'s data if the sector is present in
    /// this file: its block is allocated *and* its bitmap bit is set.
    /// `None` means the sector is not stored here (zero for a standalone
    /// dynamic VHD; "ask the parent layer" in a CCBoot increment chain).
    pub(crate) fn sector_offset(&self, sector: u64) -> io::Result<Option<u64>> {
        let sectors_per_block = self.block_size / SECTOR;
        let block = (sector / sectors_per_block) as usize;
        let entry = *self
            .bat
            .get(block)
            .ok_or_else(|| invalid_data("sector beyond BAT"))?;
        if entry == 0xFFFF_FFFF {
            return Ok(None);
        }
        let sector_in_block = sector % sectors_per_block;
        let bitmap = self.bitmaps[block]
            .as_ref()
            .ok_or_else(|| invalid_data("allocated block without bitmap"))?;
        let byte = bitmap[(sector_in_block / 8) as usize];
        if byte & (1 << (7 - sector_in_block % 8)) == 0 {
            return Ok(None);
        }
        Ok(Some(
            entry as u64 * SECTOR + self.bitmap_size + sector_in_block * SECTOR,
        ))
    }

    /// Physical byte offset of `sector`'s data if its *block* is allocated in
    /// this file, **ignoring the per-sector bitmap**. `None` only when the block
    /// itself is a hole. Used as a fallback in CCBoot chains: CCBoot allocates a
    /// whole block when it captures any change in it, but does not always set the
    /// bitmap bit for every sector it actually wrote — so a bit-clear sector in
    /// an allocated block still holds that layer's data and must not fall through
    /// to the (stale) base.
    pub(crate) fn block_sector_offset(&self, sector: u64) -> io::Result<Option<u64>> {
        let sectors_per_block = self.block_size / SECTOR;
        let block = (sector / sectors_per_block) as usize;
        let entry = *self
            .bat
            .get(block)
            .ok_or_else(|| invalid_data("sector beyond BAT"))?;
        if entry == 0xFFFF_FFFF {
            return Ok(None);
        }
        let sector_in_block = sector % sectors_per_block;
        Ok(Some(
            entry as u64 * SECTOR + self.bitmap_size + sector_in_block * SECTOR,
        ))
    }

    /// Read raw bytes at a physical file offset (as returned by `sector_offset`).
    pub(crate) fn read_phys(&self, offset: u64, dst: &mut [u8]) -> io::Result<()> {
        read_exact_at(&self.file, offset, dst)
    }
}

impl BackingStore for DynamicVhd {
    fn size_bytes(&self) -> u64 {
        self.virtual_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        // Allocation unit = one 512-byte sector (honors the per-sector bitmap).
        read_units(
            offset,
            buf,
            self.virtual_size,
            SECTOR,
            |sector| {
                Ok(match self.sector_offset(sector)? {
                    None => Unit::Zero,
                    Some(off) => Unit::At(off),
                })
            },
            |phys_off, dst| read_exact_at(&self.file, phys_off, dst),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;
    use crate::storage::BackingStore;
    use proptest::prelude::*;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    // Reference blob: 3 blocks of 4096; middle block all zero (left unallocated).
    fn sample_blob() -> Vec<u8> {
        let mut data = vec![0u8; 4096 * 3];
        for i in 0..4096 {
            data[i] = (i % 251) as u8; // block 0
            data[4096 * 2 + i] = (i % 97) as u8 + 1; // block 2 (nonzero)
        }
        data
    }

    #[test]
    fn reads_present_and_unallocated_blocks() {
        let blob = sample_blob();
        let img = fixtures::dynamic_vhd(&blob, 4096);
        let tmp = write_tmp(&img);
        let store = DynamicVhd::open(tmp.path()).unwrap();

        assert_eq!(store.size_bytes(), blob.len() as u64);
        let mut buf = vec![0u8; blob.len()];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, blob);

        // The unallocated middle block reads as zeros.
        let mut mid = vec![0xAAu8; 4096];
        store.read_at(4096, &mut mid).unwrap();
        assert!(mid.iter().all(|&b| b == 0));
    }

    #[test]
    fn read_spanning_block_boundary() {
        let blob = sample_blob();
        let img = fixtures::dynamic_vhd(&blob, 4096);
        let tmp = write_tmp(&img);
        let store = DynamicVhd::open(tmp.path()).unwrap();

        // 100 bytes straddling the block0/block1 boundary.
        let mut buf = vec![0u8; 100];
        store.read_at(4096 - 50, &mut buf).unwrap();
        assert_eq!(&buf[..], &blob[4096 - 50..4096 + 50]);
    }

    proptest! {
        // Reading any sub-range of a dynamic VHD equals the raw reference blob.
        #[test]
        fn reads_match_reference(
            seed in any::<u64>(),
            offset in 0usize..(4096 * 4),
            len in 0usize..512,
        ) {
            // Build a 4-block blob deterministically from the seed; some blocks zero.
            let mut blob = vec![0u8; 4096 * 4];
            let mut x = seed | 1;
            for (i, b) in blob.iter_mut().enumerate() {
                // Zero out whole block 1 and block 3 sometimes to hit holes.
                let block = i / 4096;
                if (block == 1 && seed & 1 == 0) || (block == 3 && seed & 2 == 0) {
                    continue;
                }
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                *b = (x & 0xFF) as u8;
            }
            let img = fixtures::dynamic_vhd(&blob, 4096);
            let tmp = write_tmp(&img);
            let store = DynamicVhd::open(tmp.path()).unwrap();

            let end = (offset + len).min(blob.len());
            let off = offset.min(end);
            let mut buf = vec![0u8; end - off];
            store.read_at(off as u64, &mut buf).unwrap();
            prop_assert_eq!(&buf[..], &blob[off..end]);
        }
    }
}
