//! Dynamic VHDX backing store (read-only, clean image / empty log).

use std::fs::File;
use std::io;
use std::path::Path;

mod crc32c;
mod metadata;
pub mod structs;

use crate::storage::blockmap::{read_units, Unit};
use crate::storage::file_ext::read_exact_at;
use crate::storage::{invalid_data, BackingStore};
use metadata::parse_metadata;
use structs::{parse_best_header, parse_region_table};

const PB_STATE_FULLY_PRESENT: u64 = 6;
const PB_STATE_PARTIALLY_PRESENT: u64 = 7;

pub struct Vhdx {
    file: File,
    virtual_size: u64,
    block_size: u64,
    chunk_ratio: u64,
    /// Raw BAT entries (payload + interleaved sector-bitmap entries).
    bat: Vec<u64>,
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl Vhdx {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;

        // File identifier.
        let mut ident = [0u8; 8];
        read_exact_at(&file, 0, &mut ident)?;
        if &ident != b"vhdxfile" {
            return Err(invalid_data("not a VHDX file"));
        }

        // Headers at 64 KiB and 128 KiB.
        let mut h1 = vec![0u8; 4096];
        let mut h2 = vec![0u8; 4096];
        read_exact_at(&file, 64 * 1024, &mut h1)?;
        read_exact_at(&file, 128 * 1024, &mut h2)?;
        let _header = parse_best_header(&h1, &h2)?;

        // Region table at 192 KiB (copy at 256 KiB).
        let mut rt = vec![0u8; 64 * 1024];
        if read_exact_at(&file, 192 * 1024, &mut rt)
            .and_then(|_| parse_region_table(&rt))
            .is_err()
        {
            read_exact_at(&file, 256 * 1024, &mut rt)?;
        }
        let regions = parse_region_table(&rt)?;
        let (meta_off, meta_len) = regions
            .metadata
            .ok_or_else(|| invalid_data("VHDX missing metadata region"))?;
        let (bat_off, bat_len) = regions
            .bat
            .ok_or_else(|| invalid_data("VHDX missing BAT region"))?;

        // Metadata.
        let mut meta = vec![0u8; meta_len as usize];
        read_exact_at(&file, meta_off, &mut meta)?;
        let md = parse_metadata(&meta)?;

        // chunk_ratio = (2^23 * logical_sector_size) / block_size.
        let chunk_ratio = (8_388_608u64 * md.logical_sector_size) / md.block_size;
        if chunk_ratio == 0 {
            return Err(invalid_data("invalid VHDX chunk ratio"));
        }

        // BAT.
        let mut raw = vec![0u8; bat_len as usize];
        read_exact_at(&file, bat_off, &mut raw)?;
        let bat: Vec<u64> = raw.chunks_exact(8).map(le_u64).collect();

        Ok(Self {
            file,
            virtual_size: md.virtual_size,
            block_size: md.block_size,
            chunk_ratio,
            bat,
        })
    }
}

impl BackingStore for Vhdx {
    fn size_bytes(&self) -> u64 {
        self.virtual_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_units(
            offset,
            buf,
            self.virtual_size,
            self.block_size,
            |block| {
                // PB entry index skips one SB entry per chunk_ratio payload blocks.
                let idx = (block + block / self.chunk_ratio) as usize;
                let entry = *self
                    .bat
                    .get(idx)
                    .ok_or_else(|| invalid_data("block beyond BAT"))?;
                let state = entry & 0x7;
                match state {
                    PB_STATE_FULLY_PRESENT => Ok(Unit::At(entry & !0xFFFFF)),
                    PB_STATE_PARTIALLY_PRESENT => {
                        Err(invalid_data("partially-present VHDX block unsupported"))
                    }
                    _ => Ok(Unit::Zero), // not present / undefined / zero / unmapped
                }
            },
            |phys_off, dst| read_exact_at(&self.file, phys_off, dst),
        )
    }
}

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhdx::fixtures;
    use proptest::prelude::*;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    fn sample_blob() -> Vec<u8> {
        let mb = 1024 * 1024;
        let mut data = vec![0u8; mb * 3];
        for i in 0..mb {
            data[i] = (i % 251) as u8; // block 0 present
            data[mb * 2 + i] = ((i % 97) + 1) as u8; // block 2 present
        } // block 1 left zero -> not present
        data
    }

    #[test]
    fn reads_present_and_zero_blocks() {
        let blob = sample_blob();
        let tmp = write_tmp(&fixtures::dynamic_vhdx(&blob));
        let store = Vhdx::open(tmp.path()).unwrap();
        assert_eq!(store.size_bytes(), blob.len() as u64);

        let mut buf = vec![0u8; blob.len()];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, blob);

        let mut mid = vec![0xAA; 1024 * 1024];
        store.read_at(1024 * 1024, &mut mid).unwrap();
        assert!(mid.iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_non_vhdx() {
        let tmp = write_tmp(b"not a vhdx file at all............");
        assert!(Vhdx::open(tmp.path()).is_err());
    }

    proptest! {
        #[test]
        fn reads_match_reference(
            seed in any::<u64>(),
            block in 0usize..3,
            within in 0usize..(1024 * 1024),
            len in 0usize..4096,
        ) {
            let mb = 1024 * 1024;
            let mut blob = vec![0u8; mb * 3];
            let mut x = seed | 1;
            for (i, b) in blob.iter_mut().enumerate() {
                let blk = i / mb;
                if blk == 1 && seed & 1 == 0 {
                    continue; // hole
                }
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                *b = (x & 0xFF) as u8;
            }
            let tmp = write_tmp(&fixtures::dynamic_vhdx(&blob));
            let store = Vhdx::open(tmp.path()).unwrap();

            let off = (block * mb + within).min(blob.len());
            let end = (off + len).min(blob.len());
            let mut buf = vec![0u8; end - off];
            store.read_at(off as u64, &mut buf).unwrap();
            prop_assert_eq!(&buf[..], &blob[off..end]);
        }
    }
}
