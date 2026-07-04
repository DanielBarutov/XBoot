//! CCBoot-style VHD increment chains.
//!
//! CCBoot stores a master image as a base VHD plus numbered increment files
//! (`name.vhd` + `name.001.vhd` .. `name.00N.vhd`). Each increment is a plain
//! *dynamic* VHD (disk type 3, not differencing) whose per-sector bitmap marks
//! which sectors that layer overrides; parent linkage is purely the file-name
//! convention. A read must take each sector from the highest-numbered layer
//! whose bitmap claims it, falling through to the base otherwise.

use std::io;
use std::path::{Path, PathBuf};

use crate::storage::vhd::dynamic::DynamicVhd;
use crate::storage::{invalid_data, BackingStore};

const SECTOR: u64 = 512;

/// A base backing store overlaid by CCBoot increment layers (newest first).
pub struct ChainedVhd {
    overlays: Vec<DynamicVhd>,
    base: Box<dyn BackingStore>,
    virtual_size: u64,
}

impl ChainedVhd {
    /// Assemble a chain from an already-open base and increment paths ordered
    /// oldest -> newest (the `.001`, `.002`, ... order on disk).
    pub fn open(base: Box<dyn BackingStore>, increments: &[PathBuf]) -> io::Result<Self> {
        let virtual_size = base.size_bytes();
        let mut overlays = Vec::with_capacity(increments.len());
        // Newest increment wins, so probe layers in reverse file order.
        for path in increments.iter().rev() {
            let layer = DynamicVhd::open(path)?;
            if layer.size_bytes() != virtual_size {
                return Err(invalid_data(format!(
                    "increment {} virtual size {} != base {}",
                    path.display(),
                    layer.size_bytes(),
                    virtual_size
                )));
            }
            overlays.push(layer);
        }
        Ok(Self {
            overlays,
            base,
            virtual_size,
        })
    }
}

impl BackingStore for ChainedVhd {
    fn size_bytes(&self) -> u64 {
        self.virtual_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| invalid_data("read range overflows u64"))?;
        if end > self.virtual_size {
            return Err(invalid_data(format!(
                "read past end of image: {end} > {}",
                self.virtual_size
            )));
        }

        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let sector = pos / SECTOR;
            let within = (pos % SECTOR) as usize;
            let take = std::cmp::min(SECTOR as usize - within, buf.len() - done);
            let dst = &mut buf[done..done + take];

            // Resolution order for one sector:
            //  1. newest layer whose bitmap bit is set (an explicit write);
            //  2. otherwise newest layer whose *block* is allocated — CCBoot
            //     captures whole blocks without always setting every bit, so the
            //     data lives there and must win over the stale base;
            //  3. otherwise the base image.
            let mut served = false;
            for layer in &self.overlays {
                if let Some(off) = layer.sector_offset(sector)? {
                    layer.read_phys(off + within as u64, dst)?;
                    served = true;
                    break;
                }
            }
            if !served {
                for layer in &self.overlays {
                    if let Some(off) = layer.block_sector_offset(sector)? {
                        layer.read_phys(off + within as u64, dst)?;
                        served = true;
                        break;
                    }
                }
            }
            if !served {
                self.base.read_at(pos, dst)?;
            }
            pos += take as u64;
            done += take;
        }
        Ok(())
    }
}

/// Diagnostic: for one virtual byte `offset`, report what each layer (newest
/// first) and the base hold for that sector — whether the block is allocated,
/// whether the bitmap bit is set, and the first 8 raw bytes. Used to reverse-
/// engineer CCBoot's exact increment semantics.
pub fn probe(base_path: &Path, offset: u64) -> io::Result<String> {
    use std::fmt::Write as _;
    let increments = find_increments(base_path);
    let sector = offset / SECTOR;
    let mut s = String::new();
    writeln!(s, "probe offset {offset} (sector {sector}):").ok();
    // Newest first.
    for path in increments.iter().rev() {
        let layer = DynamicVhd::open(path)?;
        let bitset = layer.sector_offset(sector)?;
        let block = layer.block_sector_offset(sector)?;
        let bits = layer.block_bit_count(sector)?;
        let mut raw = [0u8; 8];
        let (alloc, bytes) = match block {
            Some(off) => {
                layer.read_phys(off, &mut raw)?;
                (true, format!("{raw:02x?}"))
            }
            None => (false, "----".to_string()),
        };
        writeln!(
            s,
            "  {:<16} alloc={} blockbits={:?} bitset={} raw8={}",
            path.file_name().unwrap().to_string_lossy(),
            alloc,
            bits,
            bitset.is_some(),
            bytes
        )
        .ok();
    }
    // Base.
    let base = crate::storage::vhd::Vhd::open(base_path)?;
    let mut raw = [0u8; 8];
    base.read_at(offset, &mut raw)?;
    writeln!(s, "  {:<16} base raw8={:02x?}", "BASE", raw).ok();
    Ok(s)
}

/// Find CCBoot increment files next to `base` (`stem.001.ext`, `stem.002.ext`,
/// ...), matched case-insensitively, returned sorted oldest -> newest.
pub fn find_increments(base: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem), Some(ext)) = (
        base.parent(),
        base.file_stem().and_then(|s| s.to_str()),
        base.extension().and_then(|s| s.to_str()),
    ) else {
        return Vec::new();
    };

    let stem_lc = stem.to_lowercase();
    let ext_lc = ext.to_lowercase();
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    }) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let name_lc = name.to_lowercase();
        // Expect "<stem>.<digits>.<ext>".
        let Some(rest) = name_lc.strip_prefix(&stem_lc) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix('.') else {
            continue;
        };
        let Some(num) = rest.strip_suffix(&ext_lc) else {
            continue;
        };
        let Some(num) = num.strip_suffix('.') else {
            continue;
        };
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = num.parse::<u32>() {
            found.push((n, entry.path()));
        }
    }
    found.sort_by_key(|&(n, _)| n);
    found.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;

    const BS: u32 = 4096; // 8 sectors per block

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xboot-chain-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sector_fill(data: &mut [u8], sector: u64, byte: u8) {
        let s = sector as usize * 512;
        data[s..s + 512].fill(byte);
    }

    /// Two 8-sector blocks (BS = 4096). Models a realistic CCBoot chain where
    /// each increment captures whole blocks (carrying the then-current data for
    /// every sector) and only sets bitmap bits for the sectors it explicitly
    /// wrote.
    ///
    /// base : all 0xB0, except sector 0 = 0xB1.
    /// .001 : captures block0; bit-set sector 1 = 0x11; rest of block0 = base.
    /// .002 : captures block0 and block1; bit-set sector 1 = 0x22, sector 9 =
    ///        0x22; sector 8 (bit CLEAR) = 0xC8 to prove block data — not the
    ///        base — fills bit-clear sectors of an allocated block.
    fn build_chain(dir: &Path) -> ChainedVhd {
        let mut base_data = vec![0xB0u8; BS as usize * 2]; // 16 sectors
        sector_fill(&mut base_data, 0, 0xB1);
        let base_path = dir.join("img.vhd");
        std::fs::write(&base_path, fixtures::dynamic_vhd(&base_data, BS)).unwrap();

        // .001 captures block0 only.
        let mut l1 = base_data.clone();
        sector_fill(&mut l1, 1, 0x11);
        // Block1 of l1 is irrelevant (unallocated); zero it for clarity.
        l1[BS as usize..].fill(0);
        let p1 = dir.join("img.001.vhd");
        std::fs::write(&p1, fixtures::diff_vhd(&l1, BS, &[1])).unwrap();

        // .002 captures block0 and block1.
        let mut l2 = base_data.clone();
        sector_fill(&mut l2, 1, 0x22);
        sector_fill(&mut l2, 9, 0x22);
        sector_fill(&mut l2, 8, 0xC8); // present in block but bit stays clear
        let p2 = dir.join("img.002.vhd");
        std::fs::write(&p2, fixtures::diff_vhd(&l2, BS, &[1, 9])).unwrap();

        let base = crate::storage::vhd::Vhd::open(&base_path).unwrap();
        ChainedVhd::open(base, &[p1, p2]).unwrap()
    }

    #[test]
    fn newest_bit_set_layer_wins_then_base() {
        let dir = tmp_dir("basic");
        let chain = build_chain(&dir);

        let mut buf = vec![0u8; 512];
        chain.read_at(0, &mut buf).unwrap(); // sector 0: unchanged, 0xB1
        assert!(
            buf.iter().all(|&b| b == 0xB1),
            "sector 0 = 0x{:02X}",
            buf[0]
        );

        chain.read_at(512, &mut buf).unwrap(); // sector 1: layer2 bit-set beats layer1
        assert!(
            buf.iter().all(|&b| b == 0x22),
            "sector 1 = 0x{:02X}",
            buf[0]
        );

        chain.read_at(9 * 512, &mut buf).unwrap(); // sector 9: only layer2
        assert!(
            buf.iter().all(|&b| b == 0x22),
            "sector 9 = 0x{:02X}",
            buf[0]
        );

        chain.read_at(2 * 512, &mut buf).unwrap(); // sector 2: unchanged, 0xB0
        assert!(
            buf.iter().all(|&b| b == 0xB0),
            "sector 2 = 0x{:02X}",
            buf[0]
        );
    }

    #[test]
    fn clear_bit_in_allocated_block_reads_block_data_not_base() {
        // Sector 8 lives in block1, which only layer2 captured. Its bitmap bit is
        // clear, but layer2's block carries 0xC8 there. The read must return that
        // block data (0xC8), NOT the base's 0xB0 and NOT zero. This is the CCBoot
        // semantic: a captured block owns its bit-clear sectors over the base.
        let dir = tmp_dir("blockdata");
        let chain = build_chain(&dir);

        let mut buf = vec![0u8; 512];
        chain.read_at(8 * 512, &mut buf).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0xC8),
            "sector 8 must read layer2 block data 0xC8, got 0x{:02X}",
            buf[0]
        );
    }

    #[test]
    fn sector_in_unallocated_block_falls_through_to_base() {
        // Sector 10 is in block1; layer2 captured block1, so it is owned by
        // layer2's block data (= base 0xB0 there). Sector 7 is in block0; both
        // layers captured block0, newest (layer2) block data = base 0xB0.
        // Use a fresh chain where block1 is captured by NO layer to exercise the
        // pure base path.
        let dir = tmp_dir("baseonly");
        let mut base_data = vec![0xB0u8; BS as usize * 2];
        sector_fill(&mut base_data, 12, 0xBC);
        let base_path = dir.join("b.vhd");
        std::fs::write(&base_path, fixtures::dynamic_vhd(&base_data, BS)).unwrap();
        // Increment captures block0 only.
        let mut l1 = base_data.clone();
        sector_fill(&mut l1, 1, 0x11);
        let p1 = dir.join("b.001.vhd");
        std::fs::write(&p1, fixtures::diff_vhd(&l1, BS, &[1])).unwrap();
        let base = crate::storage::vhd::Vhd::open(&base_path).unwrap();
        let chain = ChainedVhd::open(base, &[p1]).unwrap();

        let mut buf = vec![0u8; 512];
        chain.read_at(12 * 512, &mut buf).unwrap(); // block1 unallocated -> base
        assert!(
            buf.iter().all(|&b| b == 0xBC),
            "sector 12 = 0x{:02X}",
            buf[0]
        );
    }

    #[test]
    fn read_spanning_layers() {
        let dir = tmp_dir("span");
        let chain = build_chain(&dir);

        // Sectors 0..3: 0xB1 (unchanged), 0x22 (layer2 write), 0xB0 (unchanged).
        let mut buf = vec![0u8; 512 * 3];
        chain.read_at(0, &mut buf).unwrap();
        assert!(buf[..512].iter().all(|&b| b == 0xB1));
        assert!(buf[512..1024].iter().all(|&b| b == 0x22));
        assert!(buf[1024..].iter().all(|&b| b == 0xB0));
    }

    #[test]
    fn size_mismatch_rejected() {
        let dir = tmp_dir("mismatch");
        let base_path = dir.join("m.vhd");
        std::fs::write(
            &base_path,
            fixtures::dynamic_vhd(&vec![1u8; BS as usize], BS),
        )
        .unwrap();
        let p1 = dir.join("m.001.vhd");
        std::fs::write(
            &p1,
            fixtures::diff_vhd(&vec![0u8; BS as usize * 2], BS, &[0]),
        )
        .unwrap();
        let base = crate::storage::vhd::Vhd::open(&base_path).unwrap();
        assert!(ChainedVhd::open(base, &[p1]).is_err());
    }

    #[test]
    fn finds_increments_sorted_and_case_insensitive() {
        let dir = tmp_dir("find");
        let base = dir.join("disk.VHD");
        std::fs::write(&base, b"x").unwrap();
        std::fs::write(dir.join("disk.002.vhd"), b"x").unwrap();
        std::fs::write(dir.join("DISK.001.VHD"), b"x").unwrap();
        std::fs::write(dir.join("disk.010.vhd"), b"x").unwrap();
        std::fs::write(dir.join("disk.other.vhd"), b"x").unwrap();
        std::fs::write(dir.join("disk2.001.vhd"), b"x").unwrap();
        std::fs::write(dir.join("disk.001.vhd.bak"), b"x").unwrap();

        let incs = find_increments(&base);
        let names: Vec<String> = incs
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_lowercase())
            .collect();
        assert_eq!(names, vec!["disk.001.vhd", "disk.002.vhd", "disk.010.vhd"]);
    }

    #[test]
    fn no_increments_for_plain_image() {
        let dir = tmp_dir("none");
        let base = dir.join("solo.vhd");
        std::fs::write(&base, b"x").unwrap();
        assert!(find_increments(&base).is_empty());
    }
}
