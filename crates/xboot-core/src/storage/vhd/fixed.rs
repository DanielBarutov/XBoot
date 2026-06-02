use std::fs::File;
use std::io;
use std::path::Path;

use crate::storage::file_ext::read_exact_at;
use crate::storage::vhd::footer::{VhdFooter, DISK_TYPE_FIXED};
use crate::storage::{invalid_data, BackingStore};

/// Fixed VHD: raw data followed by a 512-byte footer. The virtual size is the
/// footer's `current_size`; data starts at byte 0.
pub struct FixedVhd {
    file: File,
    virtual_size: u64,
}

impl FixedVhd {
    /// Open a fixed VHD read-only. Validates the trailing footer.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < 512 {
            return Err(invalid_data("VHD file shorter than a footer"));
        }
        let mut footer_buf = [0u8; 512];
        read_exact_at(&file, file_len - 512, &mut footer_buf)?;
        let footer = VhdFooter::parse(&footer_buf)?;
        if footer.disk_type != DISK_TYPE_FIXED {
            return Err(invalid_data("not a fixed VHD"));
        }
        Ok(Self {
            file,
            virtual_size: footer.current_size,
        })
    }
}

impl BackingStore for FixedVhd {
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
        read_exact_at(&self.file, offset, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;
    use crate::storage::BackingStore;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn reads_data_excluding_footer() {
        let mut data = vec![0u8; 2048];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let img = fixtures::fixed_vhd(&data);
        let tmp = write_tmp(&img);
        let store = FixedVhd::open(tmp.path()).unwrap();

        assert_eq!(store.size_bytes(), 2048); // virtual size, footer excluded
        let mut buf = vec![0u8; 2048];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn read_past_virtual_size_errors() {
        let img = fixtures::fixed_vhd(&[0u8; 512]);
        let tmp = write_tmp(&img);
        let store = FixedVhd::open(tmp.path()).unwrap();
        let mut buf = [0u8; 513];
        assert!(store.read_at(0, &mut buf).is_err());
    }
}
