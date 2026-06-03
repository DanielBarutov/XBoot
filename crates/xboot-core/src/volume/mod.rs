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
    /// Write `buf` starting at byte `offset` into the overlay, block by block.
    /// A block fully covered by the range is stored directly; a partially
    /// covered block is read-modified-written (from the overlay if present,
    /// else from the master) so untouched bytes are preserved.
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.check_bounds(offset, buf.len())?;
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let blk = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let blen = self.block_len(blk);
            let take = std::cmp::min(blen - within, buf.len() - done);
            let src = &buf[done..done + take];

            if take == blen {
                self.overlay.write_block(blk, src);
            } else {
                let mut block = vec![0u8; blen];
                if !self.overlay.read_block(blk, &mut block) {
                    self.master.read_at(blk * BLOCK, &mut block)?;
                }
                block[within..within + take].copy_from_slice(src);
                self.overlay.write_block(blk, &block);
            }
            pos += take as u64;
            done += take;
        }
        Ok(())
    }

    /// Discard the client's overlay (volatile reboot); reads return the master.
    pub fn reset(&self) {
        self.overlay.reset();
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;

    /// In-memory backing store for tests: the bytes ARE the disk.
    pub struct Mem(pub Vec<u8>);

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
}

#[cfg(test)]
mod tests {
    use super::tests_support::Mem;
    use super::*;

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

    #[test]
    fn write_full_block_then_read() {
        let v = vol(ramp(10_000));
        let data = [0xCD; BLOCK as usize];
        v.write_at(0, &data).unwrap();
        let mut buf = [0u8; BLOCK as usize];
        v.read_at(0, &mut buf).unwrap();
        assert_eq!(buf.to_vec(), data.to_vec());
    }

    #[test]
    fn write_partial_new_block_does_rmw() {
        // Write 4 bytes in the middle of block 0 (not yet in overlay).
        // The rest of the block must still read back as the master's bytes.
        let v = vol(ramp(10_000));
        v.write_at(10, &[0xEE; 4]).unwrap();
        let mut buf = [0u8; 20];
        v.read_at(0, &mut buf).unwrap();
        let mut expect: Vec<u8> = (0..20u8).collect();
        expect[10..14].copy_from_slice(&[0xEE; 4]);
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn write_partial_existing_block_patches() {
        let v = vol(ramp(10_000));
        v.write_at(0, &[0x11; 8]).unwrap(); // block 0 now in overlay (partial)
        v.write_at(2, &[0x22; 2]).unwrap(); // patch existing overlay block
        let mut buf = [0u8; 8];
        v.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0x11, 0x11, 0x22, 0x22, 0x11, 0x11, 0x11, 0x11]);
    }

    #[test]
    fn write_spans_two_blocks() {
        let v = vol(ramp(10_000));
        // 8-byte write straddling the block 0/1 boundary.
        v.write_at(BLOCK - 4, &[0x33; 8]).unwrap();
        let mut buf = [0u8; 8];
        v.read_at(BLOCK - 4, &mut buf).unwrap();
        assert_eq!(buf, [0x33; 8]);
        // bytes just before and after are untouched master bytes
        let mut before = [0u8; 1];
        v.read_at(BLOCK - 5, &mut before).unwrap();
        assert_eq!(before[0], ((BLOCK as usize - 5) % 256) as u8);
    }

    #[test]
    fn write_tail_block() {
        let n = BLOCK as usize + 100;
        let v = vol(ramp(n));
        v.write_at(BLOCK + 10, &[0x44; 5]).unwrap();
        let mut buf = [0u8; 100];
        v.read_at(BLOCK, &mut buf).unwrap();
        let mut expect: Vec<u8> = (0..100).map(|i| ((BLOCK as usize + i) % 256) as u8).collect();
        expect[10..15].copy_from_slice(&[0x44; 5]);
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn write_zero_len_is_ok() {
        let v = vol(ramp(10));
        v.write_at(10, &[]).unwrap();
    }

    #[test]
    fn write_out_of_range_errors() {
        let v = vol(ramp(10));
        assert!(v.write_at(8, &[0u8; 5]).is_err());
    }

    #[test]
    fn write_never_touches_master_bytes_outside_overlay() {
        // After a partial write, reading an untouched later block returns master.
        let v = vol(ramp(10_000));
        v.write_at(0, &[0x55; 4]).unwrap();
        let mut buf = [0u8; 4];
        v.read_at(5000, &mut buf).unwrap();
        let expect: Vec<u8> = (5000..5004).map(|i| (i % 256) as u8).collect();
        assert_eq!(buf.to_vec(), expect);
    }

    use std::sync::Arc;

    /// Wrapper to share one master between several Volumes via Arc.
    struct ArcStore(Arc<dyn BackingStore>);
    impl BackingStore for ArcStore {
        fn size_bytes(&self) -> u64 {
            self.0.size_bytes()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            self.0.read_at(offset, buf)
        }
    }

    #[test]
    fn reset_discards_writes_and_restores_master() {
        let v = vol(ramp(10_000));
        v.write_at(0, &[0x77; 16]).unwrap();
        v.reset();
        let mut buf = [0u8; 16];
        v.read_at(0, &mut buf).unwrap();
        let expect: Vec<u8> = (0..16u8).collect();
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn two_volumes_share_master_but_isolate_writes() {
        let master: Arc<dyn BackingStore> = Arc::new(Mem(ramp(10_000)));
        let a = Volume::new(Box::new(ArcStore(master.clone())), Box::new(RamOverlay::new()));
        let b = Volume::new(Box::new(ArcStore(master.clone())), Box::new(RamOverlay::new()));

        a.write_at(0, &[0x99; 8]).unwrap();

        // b never sees a's write.
        let mut buf = [0u8; 8];
        b.read_at(0, &mut buf).unwrap();
        assert_eq!(buf.to_vec(), (0..8u8).collect::<Vec<_>>());

        // a does see its own write.
        a.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0x99; 8]);
    }
}

#[cfg(test)]
mod prop {
    use super::tests_support::Mem;
    use super::*;
    use proptest::collection as pcoll;
    use proptest::prelude::*;

    /// One operation against both the Volume and the reference byte array.
    #[derive(Debug, Clone)]
    enum Op {
        Read { offset: usize, len: usize },
        Write { offset: usize, bytes: Vec<u8> },
    }

    const SIZE: usize = 3 * BLOCK as usize + 123; // a few blocks + a short tail

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..SIZE, 0..(2 * BLOCK as usize)).prop_map(|(offset, len)| {
                let len = len.min(SIZE - offset);
                Op::Read { offset, len }
            }),
            (0..SIZE, pcoll::vec(any::<u8>(), 0..(2 * BLOCK as usize))).prop_map(
                |(offset, mut bytes)| {
                    bytes.truncate(SIZE - offset);
                    Op::Write { offset, bytes }
                }
            ),
        ]
    }

    proptest! {
        #[test]
        fn volume_matches_flat_model(ops in pcoll::vec(op_strategy(), 0..200)) {
            // Master is a fixed ramp; model starts equal to it.
            let master: Vec<u8> = (0..SIZE).map(|i| (i % 256) as u8).collect();
            let mut model = master.clone();
            let v = Volume::new(Box::new(Mem(master)), Box::new(RamOverlay::new()));

            for op in ops {
                match op {
                    Op::Read { offset, len } => {
                        let mut got = vec![0u8; len];
                        v.read_at(offset as u64, &mut got).unwrap();
                        prop_assert_eq!(&got, &model[offset..offset + len]);
                    }
                    Op::Write { offset, bytes } => {
                        v.write_at(offset as u64, &bytes).unwrap();
                        model[offset..offset + bytes.len()].copy_from_slice(&bytes);
                    }
                }
            }

            // Final full read must equal the model.
            let mut full = vec![0u8; SIZE];
            v.read_at(0, &mut full).unwrap();
            prop_assert_eq!(full, model);
        }
    }
}
