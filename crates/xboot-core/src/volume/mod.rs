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
}
