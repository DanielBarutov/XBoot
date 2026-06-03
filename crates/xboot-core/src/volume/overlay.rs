use std::collections::HashMap;
use std::sync::RwLock;

/// COW block size in bytes. The overlay stores data in blocks of this size
/// (the disk's tail block may be shorter).
pub const BLOCK: u64 = 4096;

/// Per-client store of overwritten blocks. Access is copy-based to avoid
/// lock-lifetime borrow issues. Volatile: `reset` discards everything.
pub trait OverlayStore: Send + Sync {
    /// If block `blk` exists, fill `buf` with its data and return true.
    /// `buf.len()` must equal the stored block's length.
    fn read_block(&self, blk: u64, buf: &mut [u8]) -> bool;
    /// Store/overwrite block `blk` with exactly `data`.
    fn write_block(&self, blk: u64, data: &[u8]);
    /// Discard all overlay contents (volatile "reboot").
    fn reset(&self);
}

/// RAM implementation of `OverlayStore` backed by a `HashMap` under an `RwLock`.
#[derive(Default)]
pub struct RamOverlay {
    blocks: RwLock<HashMap<u64, Box<[u8]>>>,
}

impl RamOverlay {
    pub fn new() -> Self {
        Self::default()
    }
}

impl OverlayStore for RamOverlay {
    fn read_block(&self, blk: u64, buf: &mut [u8]) -> bool {
        let map = self.blocks.read().unwrap();
        match map.get(&blk) {
            Some(data) => {
                buf.copy_from_slice(data);
                true
            }
            None => false,
        }
    }

    fn write_block(&self, blk: u64, data: &[u8]) {
        self.blocks
            .write()
            .unwrap()
            .insert(blk, data.to_vec().into_boxed_slice());
    }

    fn reset(&self) {
        self.blocks.write().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miss_returns_false_and_leaves_buf() {
        let ov = RamOverlay::new();
        let mut buf = [9u8; 4];
        assert!(!ov.read_block(0, &mut buf));
        assert_eq!(buf, [9, 9, 9, 9]);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let ov = RamOverlay::new();
        ov.write_block(7, &[1, 2, 3, 4]);
        let mut buf = [0u8; 4];
        assert!(ov.read_block(7, &mut buf));
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn write_overwrites_existing() {
        let ov = RamOverlay::new();
        ov.write_block(7, &[1, 2, 3, 4]);
        ov.write_block(7, &[5, 6, 7, 8]);
        let mut buf = [0u8; 4];
        assert!(ov.read_block(7, &mut buf));
        assert_eq!(buf, [5, 6, 7, 8]);
    }

    #[test]
    fn reset_clears_all() {
        let ov = RamOverlay::new();
        ov.write_block(0, &[1; 4]);
        ov.write_block(1, &[2; 4]);
        ov.reset();
        let mut buf = [0u8; 4];
        assert!(!ov.read_block(0, &mut buf));
        assert!(!ov.read_block(1, &mut buf));
    }
}
