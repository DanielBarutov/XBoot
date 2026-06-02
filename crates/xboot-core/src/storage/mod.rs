use std::io;
use std::path::Path;

mod blockmap;
mod file_ext;
mod raw;
mod vhd;
mod vhdx;

pub use raw::RawFile;
pub use vhd::FixedVhd;
pub use vhd::Vhd;
pub use vhdx::Vhdx;

/// A read-only source of disk-image bytes (raw, VHD, VHDX, ...).
pub trait BackingStore: Send + Sync {
    /// Total virtual size in bytes.
    fn size_bytes(&self) -> u64;

    /// Read exactly `buf.len()` bytes starting at byte `offset` into `buf`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// Build an `InvalidData` I/O error from a message. Parsers use this so the
/// `BackingStore` boundary stays `io::Result` without a separate error enum.
pub(crate) fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Open a master image, detecting the format by magic bytes.
/// Implemented in the final task; stubbed so the crate compiles meanwhile.
pub fn open_backing(_path: &Path) -> io::Result<Box<dyn BackingStore>> {
    Err(invalid_data("open_backing not implemented yet"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InMemory(Vec<u8>);

    impl BackingStore for InMemory {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let start = offset as usize;
            buf.copy_from_slice(&self.0[start..start + buf.len()]);
            Ok(())
        }
    }

    #[test]
    fn trait_is_object_safe_and_usable() {
        let store: Box<dyn BackingStore> = Box::new(InMemory(vec![1, 2, 3, 4]));
        let mut buf = [0u8; 2];
        store.read_at(1, &mut buf).unwrap();
        assert_eq!(buf, [2, 3]);
        assert_eq!(store.size_bytes(), 4);
    }
}
