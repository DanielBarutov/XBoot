use std::fs::File;
use std::io;
use std::path::Path;

use super::file_ext::read_exact_at;
use super::{invalid_data, BackingStore};

/// A backing store over a raw disk image: the file *is* the disk, byte-for-byte.
pub struct RawFile {
    file: File,
    size: u64,
}

impl RawFile {
    /// Open a raw image read-only.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl BackingStore for RawFile {
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| invalid_data("read range overflows u64"))?;
        if end > self.size {
            return Err(invalid_data(format!(
                "read past end of image: {end} > {}",
                self.size
            )));
        }
        read_exact_at(&self.file, offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::BackingStore;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn size_and_read() {
        let f = write_tmp(b"0123456789");
        let store = RawFile::open(f.path()).unwrap();
        assert_eq!(store.size_bytes(), 10);
        let mut buf = [0u8; 4];
        store.read_at(3, &mut buf).unwrap();
        assert_eq!(&buf, b"3456");
    }

    #[test]
    fn read_past_end_errors() {
        let f = write_tmp(b"abc");
        let store = RawFile::open(f.path()).unwrap();
        let mut buf = [0u8; 4];
        assert!(store.read_at(0, &mut buf).is_err());
    }

    #[test]
    fn missing_file_errors() {
        assert!(RawFile::open(std::path::Path::new("/no/such/raw.img")).is_err());
    }
}
