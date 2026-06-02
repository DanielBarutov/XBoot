/// CRC-32C (Castagnoli, polynomial 0x82F63B78 reflected), init 0xFFFFFFFF,
/// final XOR 0xFFFFFFFF — the variant VHDX uses.
// Tasks 11/12 will call this from non-test code; allow until then.
#[allow(dead_code)]
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known CRC-32C test vectors (Castagnoli, init 0xFFFFFFFF, final XOR).
    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }
}
