use std::io;

use super::invalid_data;

/// Where an allocation unit's data lives.
#[allow(dead_code)]
pub(crate) enum Unit {
    /// Unit is unallocated — reads as zeros.
    Zero,
    /// Unit's data starts at this absolute byte offset in the backing file.
    At(u64),
}

/// Read `buf.len()` bytes starting at virtual byte `offset`, splitting the range
/// across fixed-size allocation units. `locate(unit_index)` says where each unit
/// is; `read_phys(file_offset, dst)` reads `dst.len()` bytes from the file.
///
/// `offset + buf.len()` must not exceed `virtual_size`.
#[allow(dead_code)]
pub(crate) fn read_units(
    offset: u64,
    buf: &mut [u8],
    virtual_size: u64,
    unit_size: u64,
    locate: impl Fn(u64) -> io::Result<Unit>,
    read_phys: impl Fn(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<()> {
    if unit_size == 0 {
        return Err(invalid_data("unit_size must be non-zero"));
    }

    let end = offset
        .checked_add(buf.len() as u64)
        .ok_or_else(|| invalid_data("read range overflows u64"))?;
    if end > virtual_size {
        return Err(invalid_data(format!(
            "read past end of image: {end} > {virtual_size}"
        )));
    }

    let mut pos = offset;
    let mut done = 0usize;
    while done < buf.len() {
        let unit_index = pos / unit_size;
        let within = (pos % unit_size) as usize;
        let take = std::cmp::min(unit_size as usize - within, buf.len() - done);
        let dst = &mut buf[done..done + take];
        match locate(unit_index)? {
            Unit::Zero => dst.fill(0),
            Unit::At(base) => read_phys(base + within as u64, dst)?,
        }
        pos += take as u64;
        done += take;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    // A fake physical store: unit_size = 4. Unit 0 -> "AAAA", unit 1 -> hole,
    // unit 2 -> "CCCC". Backing bytes live in `phys`.
    fn run(offset: u64, len: usize) -> Vec<u8> {
        let phys: Vec<u8> = b"AAAACCCC".to_vec(); // unit0 at phys 0, unit2 at phys 4
        let mut buf = vec![0u8; len];
        read_units(
            offset,
            &mut buf,
            12, // virtual size: 3 units * 4
            4,
            |unit| {
                Ok(match unit {
                    0 => Unit::At(0),
                    1 => Unit::Zero,
                    2 => Unit::At(4),
                    _ => Unit::Zero,
                })
            },
            |phys_off, dst| {
                let s = phys_off as usize;
                dst.copy_from_slice(&phys[s..s + dst.len()]);
                Ok::<(), io::Error>(())
            },
        )
        .unwrap();
        buf
    }

    #[test]
    fn whole_units() {
        assert_eq!(run(0, 4), b"AAAA");
        assert_eq!(run(4, 4), b"\0\0\0\0"); // the hole
        assert_eq!(run(8, 4), b"CCCC");
    }

    #[test]
    fn spans_units_and_partial_edges() {
        // bytes 2..10 cross unit0 -> unit1(hole) -> unit2
        assert_eq!(run(2, 8), b"AA\0\0\0\0CC");
    }

    #[test]
    fn zero_unit_size_errors() {
        let mut buf = [0u8; 1];
        let r = read_units(
            0,
            &mut buf,
            12,
            0,
            |_| Ok(Unit::Zero),
            |_, _| Ok::<(), io::Error>(()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn read_past_end_errors() {
        let mut buf = [0u8; 4];
        let r = read_units(
            10,
            &mut buf,
            12,
            4,
            |_| Ok(Unit::Zero),
            |_, _| Ok::<(), io::Error>(()),
        );
        assert!(r.is_err());
    }
}
