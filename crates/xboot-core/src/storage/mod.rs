use std::io;
use std::path::Path;

mod blockmap;
mod file_ext;
mod raw;
pub mod vhd;
pub mod vhdx;

pub use raw::RawFile;
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

/// Open a master image, detecting the format by magic bytes:
/// `vhdxfile` -> VHDX; trailing `conectix` footer -> VHD (fixed/dynamic);
/// otherwise raw.
pub fn open_backing(path: &Path) -> io::Result<Box<dyn BackingStore>> {
    use file_ext::read_exact_at;

    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();

    // VHDX: 8-byte identifier at offset 0.
    if len >= 8 {
        let mut ident = [0u8; 8];
        read_exact_at(&file, 0, &mut ident)?;
        if &ident == b"vhdxfile" {
            return Ok(Box::new(Vhdx::open(path)?));
        }
    }

    // VHD: 8-byte `conectix` cookie in the trailing 512-byte footer.
    if len >= 512 {
        let mut cookie = [0u8; 8];
        read_exact_at(&file, len - 512, &mut cookie)?;
        if &cookie == b"conectix" {
            let base = Vhd::open(path)?;
            // CCBoot-style increments (`name.001.vhd`, ...) next to the base
            // override it sector-by-sector; without them clients see a stale
            // image, so they must be layered in whenever present.
            // DIAGNOSTIC: XBOOT_NO_CHAIN opens the base alone, ignoring its
            // CCBoot increments, so a single layer can be inspected.
            if std::env::var_os("XBOOT_NO_CHAIN").is_some() {
                return Ok(base);
            }
            let increments = vhd::chain::find_increments(path);
            if increments.is_empty() {
                return Ok(base);
            }
            tracing::info!(
                base = %path.display(),
                layers = increments.len(),
                newest = %increments.last().unwrap().display(),
                "vhd: layering CCBoot increment chain over base image"
            );
            return Ok(Box::new(vhd::ChainedVhd::open(base, &increments)?));
        }
    }

    // Fallback: treat as a raw image.
    Ok(Box::new(RawFile::open(path)?))
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

#[cfg(test)]
mod factory_tests {
    use super::*;
    use crate::storage::vhd::fixtures as vhd_fix;
    use crate::storage::vhdx::fixtures as vhdx_fix;

    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xboot-bs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn detects_raw() {
        let p = write_tmp("a.raw", &[1u8; 2048]);
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 2048);
    }

    #[test]
    fn detects_fixed_vhd() {
        let p = write_tmp("a.vhd", &vhd_fix::fixed_vhd(&[2u8; 1024]));
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 1024);
    }

    #[test]
    fn detects_dynamic_vhd() {
        let p = write_tmp("b.vhd", &vhd_fix::dynamic_vhd(&[3u8; 8192], 4096));
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 8192);
    }

    #[test]
    fn layers_ccboot_increments_over_base_vhd() {
        let dir = std::env::temp_dir().join(format!("xboot-bs-chain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Base: 8 sectors of 0xB0. Increment 001 captures block0 (carrying the
        // current data for every sector) and explicitly writes sector 0 = 0x11.
        let base_data = vec![0xB0u8; 4096];
        std::fs::write(dir.join("c.vhd"), vhd_fix::dynamic_vhd(&base_data, 4096)).unwrap();
        let mut inc = base_data.clone();
        inc[..512].fill(0x11);
        std::fs::write(dir.join("c.001.vhd"), vhd_fix::diff_vhd(&inc, 4096, &[0])).unwrap();

        let store = open_backing(&dir.join("c.vhd")).unwrap();
        let mut buf = [0u8; 512];
        store.read_at(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x11), "sector 0 from increment");
        store.read_at(512, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0xB0), "sector 1 unchanged in block");
    }

    #[test]
    fn detects_vhdx() {
        let p = write_tmp("a.vhdx", &vhdx_fix::dynamic_vhdx(&[4u8; 1024 * 1024]));
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 1024 * 1024);
    }
}

#[cfg(test)]
mod fuzz_smoke {
    use super::*;

    // Deterministic xorshift so failures reproduce from the printed seed.
    fn fill(seed: u64, buf: &mut [u8]) {
        let mut x = seed | 1;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x & 0xFF) as u8;
        }
    }

    #[test]
    fn open_backing_never_panics_on_garbage() {
        for seed in 0..512u64 {
            let mut bytes = vec![0u8; 4096];
            fill(seed, &mut bytes);
            // Plant magic bytes some of the time to drive the parsers deeper.
            match seed % 3 {
                0 => bytes[0..8].copy_from_slice(b"vhdxfile"),
                1 => bytes[4096 - 512..4096 - 512 + 8].copy_from_slice(b"conectix"),
                _ => {}
            }
            let dir = std::env::temp_dir().join(format!("xboot-fuzz-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("g{seed}.img"));
            std::fs::write(&p, &bytes).unwrap();
            // Must return Ok or Err, never panic.
            let _ = open_backing(&p);
        }
    }
}
