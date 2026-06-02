use std::fs::File;
use std::io;

/// Read exactly `buf.len()` bytes starting at byte `offset`, using a positioned
/// read that does not move (and is unaffected by) the file's cursor — safe to
/// call concurrently through a shared `&File`.
#[allow(dead_code)]
#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

/// Windows equivalent built on `seek_read` (which also does not move the cursor).
#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.seek_read(&mut buf[read..], offset + read as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF in positioned read",
            ));
        }
        read += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_at_offset_without_moving_cursor() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"0123456789").unwrap();
        let file = std::fs::File::open(tmp.path()).unwrap();

        let mut a = [0u8; 3];
        read_exact_at(&file, 2, &mut a).unwrap();
        assert_eq!(&a, b"234");

        // A second positioned read is unaffected by the first (no shared cursor).
        let mut b = [0u8; 2];
        read_exact_at(&file, 0, &mut b).unwrap();
        assert_eq!(&b, b"01");
    }

    #[test]
    fn reading_past_eof_errors() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"abc").unwrap();
        let file = std::fs::File::open(tmp.path()).unwrap();
        let mut buf = [0u8; 8];
        assert!(read_exact_at(&file, 0, &mut buf).is_err());
    }
}
