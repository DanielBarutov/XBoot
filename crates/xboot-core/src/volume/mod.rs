use std::io;

use crate::storage::BackingStore;

mod overlay;

pub use overlay::{OverlayStore, RamOverlay, BLOCK};

/// A client's virtual disk: a read-only master plus a volatile COW overlay.
/// Reads consult the overlay then the master. Writes always land in the
/// overlay; the master is never modified.
pub struct Volume {
    master: Box<dyn BackingStore>,
    overlay: Box<dyn OverlayStore>,
}

impl Volume {
    pub fn new(master: Box<dyn BackingStore>, overlay: Box<dyn OverlayStore>) -> Self {
        Self { master, overlay }
    }

    /// Virtual size of the volume = the master's size (the overlay never grows it).
    pub fn size_bytes(&self) -> u64 {
        self.master.size_bytes()
    }

    /// Actual length of block `blk`, accounting for a short tail block.
    /// Caller must ensure `blk * BLOCK < size_bytes`.
    fn block_len(&self, blk: u64) -> usize {
        let start = blk * BLOCK;
        std::cmp::min(BLOCK, self.size_bytes() - start) as usize
    }

    /// Reject ranges that overflow or run past the end of the volume.
    fn check_bounds(&self, offset: u64, len: usize) -> io::Result<()> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "range overflows u64"))?;
        if end > self.size_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("out of range: {end} > {}", self.size_bytes()),
            ));
        }
        Ok(())
    }

    /// Read exactly `buf.len()` bytes starting at byte `offset`, consulting the
    /// overlay first and falling back to the master, block by block.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check_bounds(offset, buf.len())?;
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let blk = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let blen = self.block_len(blk);
            let take = std::cmp::min(blen - within, buf.len() - done);
            let dst = &mut buf[done..done + take];

            let mut block = vec![0u8; blen];
            if self.overlay.read_block(blk, &mut block) {
                dst.copy_from_slice(&block[within..within + take]);
            } else {
                self.master.read_at(blk * BLOCK + within as u64, dst)?;
            }
            pos += take as u64;
            done += take;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory backing store for tests: the bytes ARE the disk.
    struct Mem(Vec<u8>);

    impl BackingStore for Mem {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let s = offset as usize;
            buf.copy_from_slice(&self.0[s..s + buf.len()]);
            Ok(())
        }
    }

    fn vol(master_bytes: Vec<u8>) -> Volume {
        Volume::new(Box::new(Mem(master_bytes)), Box::new(RamOverlay::new()))
    }

    #[test]
    fn size_is_master_size() {
        let v = vol(vec![0u8; 10_000]);
        assert_eq!(v.size_bytes(), 10_000);
    }

    // Master = 0,1,2,...,255,0,1,... over 10000 bytes.
    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 256) as u8).collect()
    }

    #[test]
    fn read_miss_returns_master() {
        let v = vol(ramp(10_000));
        let mut buf = [0u8; 5];
        v.read_at(100, &mut buf).unwrap();
        assert_eq!(buf, [100, 101, 102, 103, 104]);
    }

    #[test]
    fn read_hit_returns_overlay() {
        let v = vol(ramp(10_000));
        // Overwrite block 0 entirely with 0xAB.
        v.overlay.write_block(0, &[0xAB; BLOCK as usize]);
        let mut buf = [0u8; 4];
        v.read_at(10, &mut buf).unwrap();
        assert_eq!(buf, [0xAB; 4]);
    }

    #[test]
    fn read_spans_overlay_and_master() {
        let v = vol(ramp(10_000));
        // Overlay covers block 0 only; read crosses into block 1 (master).
        v.overlay.write_block(0, &[0xAB; BLOCK as usize]);
        let mut buf = [0u8; 8];
        v.read_at(BLOCK - 4, &mut buf).unwrap();
        // first 4 bytes from overlay (0xAB), next 4 from master at offset BLOCK..
        let mut expect = [0xABu8; 8];
        for (i, e) in expect[4..].iter_mut().enumerate() {
            *e = ((BLOCK as usize + i) % 256) as u8;
        }
        assert_eq!(buf, expect);
    }

    #[test]
    fn read_tail_block_shorter_than_block() {
        // size = 1 block + 100 bytes; read the final 100-byte tail.
        let n = BLOCK as usize + 100;
        let v = vol(ramp(n));
        let mut buf = [0u8; 100];
        v.read_at(BLOCK, &mut buf).unwrap();
        let expect: Vec<u8> = (0..100).map(|i| ((BLOCK as usize + i) % 256) as u8).collect();
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn read_zero_len_is_ok() {
        let v = vol(ramp(10));
        let mut buf: [u8; 0] = [];
        v.read_at(10, &mut buf).unwrap(); // at EOF, len 0 -> ok
    }

    #[test]
    fn read_out_of_range_errors() {
        let v = vol(ramp(10));
        let mut buf = [0u8; 5];
        assert!(v.read_at(8, &mut buf).is_err());
    }
}
