//! VHD backing stores. `Vhd::open` sniffs the footer's disk type and returns
//! the matching reader.

use std::io;
use std::path::Path;

mod dynamic;
mod fixed;
mod footer;

pub use dynamic::DynamicVhd;
pub use fixed::FixedVhd;

use crate::storage::file_ext::read_exact_at;
use crate::storage::{invalid_data, BackingStore};
use footer::{VhdFooter, DISK_TYPE_DYNAMIC, DISK_TYPE_FIXED};

/// Open a VHD (fixed or dynamic) read-only as a boxed backing store.
pub struct Vhd;

impl Vhd {
    pub fn open(path: &Path) -> io::Result<Box<dyn BackingStore>> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len < 512 {
            return Err(invalid_data("VHD file shorter than a footer"));
        }
        let mut footer_buf = [0u8; 512];
        read_exact_at(&file, len - 512, &mut footer_buf)?;
        let footer = VhdFooter::parse(&footer_buf)?;
        match footer.disk_type {
            DISK_TYPE_FIXED => Ok(Box::new(FixedVhd::open(path)?)),
            DISK_TYPE_DYNAMIC => Ok(Box::new(DynamicVhd::open(path)?)),
            other => Err(invalid_data(format!("unsupported VHD disk type {other}"))),
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn dispatches_fixed() {
        let tmp = write_tmp(&fixtures::fixed_vhd(&[5u8; 1024]));
        let store = Vhd::open(tmp.path()).unwrap();
        assert_eq!(store.size_bytes(), 1024);
    }

    #[test]
    fn dispatches_dynamic() {
        let tmp = write_tmp(&fixtures::dynamic_vhd(&[6u8; 8192], 4096));
        let store = Vhd::open(tmp.path()).unwrap();
        assert_eq!(store.size_bytes(), 8192);
    }

    #[test]
    fn rejects_differencing() {
        // disk_type 4 = differencing, unsupported.
        let mut img = fixtures::fixed_vhd(&[0u8; 512]);
        let foot = fixtures::footer(4, 512, u64::MAX);
        let n = img.len();
        img[n - 512..].copy_from_slice(&foot);
        let tmp = write_tmp(&img);
        assert!(Vhd::open(tmp.path()).is_err());
    }
}
