//! Builders that synthesize valid VHD byte images for tests.

/// Ones-complement checksum over a 512-byte footer with the checksum field
/// (bytes 64..68) treated as zero.
fn footer_checksum(footer: &[u8; 512]) -> u32 {
    let mut sum: u32 = 0;
    for (i, &b) in footer.iter().enumerate() {
        if (64..68).contains(&i) {
            continue;
        }
        sum = sum.wrapping_add(b as u32);
    }
    !sum
}

/// Build a 512-byte VHD footer. `disk_type` 2 = fixed, 3 = dynamic.
/// `data_offset` is `u64::MAX` for fixed, or the dynamic-header offset.
pub(crate) fn footer(disk_type: u32, current_size: u64, data_offset: u64) -> [u8; 512] {
    let mut f = [0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    f[8..12].copy_from_slice(&0x0000_0002u32.to_be_bytes()); // features
    f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // format version
    f[16..24].copy_from_slice(&data_offset.to_be_bytes());
    f[40..48].copy_from_slice(&current_size.to_be_bytes()); // original size
    f[48..56].copy_from_slice(&current_size.to_be_bytes()); // current size
    f[60..64].copy_from_slice(&disk_type.to_be_bytes());
    let cks = footer_checksum(&f);
    f[64..68].copy_from_slice(&cks.to_be_bytes());
    f
}

/// A fixed VHD: data (already a multiple of 512) followed by the footer.
pub(crate) fn fixed_vhd(data: &[u8]) -> Vec<u8> {
    assert!(
        data.len().is_multiple_of(512),
        "fixed VHD data must be sector-aligned"
    );
    let mut out = data.to_vec();
    out.extend_from_slice(&footer(2, data.len() as u64, u64::MAX));
    out
}

/// A dynamic VHD built from `data` (a multiple of `block_size`). Blocks whose
/// source bytes are all zero are left unallocated (BAT = 0xFFFFFFFF) to exercise
/// the zero path; other blocks are written fully present (bitmap all ones).
pub(crate) fn dynamic_vhd(data: &[u8], block_size: u32) -> Vec<u8> {
    let bs = block_size as usize;
    assert!(bs.is_multiple_of(512) && bs.is_power_of_two());
    assert!(
        data.len().is_multiple_of(bs),
        "data must be a multiple of block_size"
    );
    let virtual_size = data.len() as u64;
    let n_blocks = (data.len() / bs) as u32;
    let sectors_per_block = block_size / 512;
    let bitmap_bytes_raw = sectors_per_block.div_ceil(8);
    let bitmap_size = bitmap_bytes_raw.div_ceil(512) * 512; // pad to sector

    // Layout: footer copy (512) | dynamic header (1024) | BAT (padded 512) | blocks | footer
    let table_offset: u64 = 512 + 1024;
    let bat_bytes = (n_blocks as u64) * 4;
    let bat_padded = bat_bytes.div_ceil(512) * 512;
    let blocks_start = table_offset + bat_padded;

    let mut out = Vec::new();
    out.extend_from_slice(&footer(3, virtual_size, 512)); // footer copy at start

    // Dynamic disk header (1024 bytes).
    let mut hdr = [0u8; 1024];
    hdr[0..8].copy_from_slice(b"cxsparse");
    hdr[8..16].copy_from_slice(&u64::MAX.to_be_bytes()); // data offset
    hdr[16..24].copy_from_slice(&table_offset.to_be_bytes());
    hdr[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // header version
    hdr[28..32].copy_from_slice(&n_blocks.to_be_bytes()); // max table entries
    hdr[32..36].copy_from_slice(&block_size.to_be_bytes());
    out.extend_from_slice(&hdr);

    // Decide allocation and build the BAT.
    let mut bat = vec![0xFFFF_FFFFu32; n_blocks as usize];
    let mut next_block_sector = (blocks_start / 512) as u32;
    let block_on_disk = bitmap_size + block_size; // bytes per allocated block
    for (i, chunk) in data.chunks(bs).enumerate() {
        if chunk.iter().all(|&b| b == 0) {
            continue; // leave unallocated
        }
        bat[i] = next_block_sector;
        next_block_sector += block_on_disk / 512;
    }
    for entry in &bat {
        out.extend_from_slice(&entry.to_be_bytes());
    }
    // Pad BAT to a sector boundary.
    out.resize(blocks_start as usize, 0);

    // Write allocated blocks: bitmap (all ones for present sectors) + data.
    for (i, chunk) in data.chunks(bs).enumerate() {
        if bat[i] == 0xFFFF_FFFF {
            continue;
        }
        let mut bitmap = vec![0u8; bitmap_size as usize];
        for b in bitmap.iter_mut().take(bitmap_bytes_raw as usize) {
            *b = 0xFF;
        }
        out.extend_from_slice(&bitmap);
        out.extend_from_slice(chunk);
    }

    out.extend_from_slice(&footer(3, virtual_size, 512)); // footer at end
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_has_footer_cookie_at_end() {
        let v = fixed_vhd(&[7u8; 1024]);
        assert_eq!(&v[v.len() - 512..v.len() - 512 + 8], b"conectix");
        assert_eq!(v.len(), 1024 + 512);
    }

    #[test]
    fn dynamic_starts_with_footer_copy_and_header() {
        let v = dynamic_vhd(&[1u8; 8192], 4096);
        assert_eq!(&v[0..8], b"conectix");
        assert_eq!(&v[512..520], b"cxsparse");
    }
}
