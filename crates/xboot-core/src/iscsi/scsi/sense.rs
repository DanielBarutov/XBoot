//! SCSI status codes and fixed-format sense data (SPC-3).
#![allow(dead_code)]

// SCSI status byte values.
pub const GOOD: u8 = 0x00;
pub const CHECK_CONDITION: u8 = 0x02;

// Sense keys (byte 2, low nibble).
pub const NO_SENSE: u8 = 0x00;
pub const NOT_READY: u8 = 0x02;
pub const MEDIUM_ERROR: u8 = 0x03;
pub const ILLEGAL_REQUEST: u8 = 0x05;
pub const UNIT_ATTENTION: u8 = 0x06;

// (ASC, ASCQ) pairs we emit.
pub const ASC_INVALID_OPCODE: (u8, u8) = (0x20, 0x00);
pub const ASC_LBA_OUT_OF_RANGE: (u8, u8) = (0x21, 0x00);
pub const ASC_INVALID_FIELD_IN_CDB: (u8, u8) = (0x24, 0x00);
pub const ASC_LUN_NOT_SUPPORTED: (u8, u8) = (0x25, 0x00);
pub const ASC_UNRECOVERED_READ_ERROR: (u8, u8) = (0x11, 0x00);
pub const ASC_WRITE_ERROR: (u8, u8) = (0x0c, 0x00);

/// Build 18-byte fixed-format sense data (response code 0x70).
pub fn fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut s = vec![0u8; 18];
    s[0] = 0x70; // current error, fixed format; VALID = 0
    s[2] = key & 0x0f; // sense key
    s[7] = 10; // additional sense length (18 - 8)
    s[12] = asc;
    s[13] = ascq;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_sense_has_response_code_key_and_asc() {
        let s = fixed_sense(ILLEGAL_REQUEST, 0x20, 0x00);
        assert_eq!(s.len(), 18);
        assert_eq!(s[0], 0x70); // response code, fixed format
        assert_eq!(s[2] & 0x0f, ILLEGAL_REQUEST); // sense key
        assert_eq!(s[7], 10); // additional sense length (18 - 8)
        assert_eq!(s[12], 0x20); // ASC
        assert_eq!(s[13], 0x00); // ASCQ
    }
}
