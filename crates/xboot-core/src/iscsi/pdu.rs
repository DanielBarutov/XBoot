//! PDU decoding (initiator -> target) and response encoding (target ->
//! initiator). BHS offsets follow RFC 7143. Multi-byte fields are big-endian.

use super::text;
use super::{opcode, pad4, PduError, BHS_LEN, MAX_DATA_SEGMENT};

// ---- big-endian readers over a >=48-byte slice; offsets are fixed <= 44 ----

fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}
fn be24(b: &[u8], off: usize) -> usize {
    ((b[off] as usize) << 16) | ((b[off + 1] as usize) << 8) | (b[off + 2] as usize)
}
fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn be64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

// ---- request types (initiator -> target) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Login(LoginRequest),
    Text(TextRequest),
    ScsiCommand(ScsiCommand),
    DataOut(ScsiDataOut),
    NopOut(NopOut),
    Logout(LogoutRequest),
    TaskMgmt(TaskMgmt),
    /// Any opcode we do not handle. 05c answers with a Reject; never an error.
    Unsupported {
        opcode: u8,
        itt: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRequest {
    pub transit: bool,
    pub continue_: bool,
    pub csg: u8,
    pub nsg: u8,
    pub version_max: u8,
    pub version_min: u8,
    pub isid: [u8; 6],
    pub tsih: u16,
    pub itt: u32,
    pub cid: u16,
    pub cmd_sn: u32,
    pub exp_stat_sn: u32,
    pub text: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRequest {
    pub final_: bool,
    pub continue_: bool,
    pub lun: u64,
    pub itt: u32,
    pub ttt: u32,
    pub cmd_sn: u32,
    pub exp_stat_sn: u32,
    pub text: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScsiCommand {
    pub final_: bool,
    pub read: bool,
    pub write: bool,
    pub attr: u8,
    pub lun: u64,
    pub itt: u32,
    pub edtl: u32,
    pub cmd_sn: u32,
    pub exp_stat_sn: u32,
    pub cdb: [u8; 16],
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScsiDataOut {
    pub final_: bool,
    pub lun: u64,
    pub itt: u32,
    pub ttt: u32,
    pub exp_stat_sn: u32,
    pub data_sn: u32,
    pub buffer_offset: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NopOut {
    pub lun: u64,
    pub itt: u32,
    pub ttt: u32,
    pub cmd_sn: u32,
    pub exp_stat_sn: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutRequest {
    pub reason: u8,
    pub itt: u32,
    pub cid: u16,
    pub cmd_sn: u32,
    pub exp_stat_sn: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskMgmt {
    pub function: u8,
    pub lun: u64,
    pub itt: u32,
    pub ref_task_tag: u32,
    pub cmd_sn: u32,
    pub exp_stat_sn: u32,
}

/// Decode one PDU from the front of `buf`. Returns the parsed request and the
/// total number of bytes consumed (BHS + data + padding). Never panics.
pub fn decode(buf: &[u8]) -> Result<(Request, usize), PduError> {
    if buf.len() < BHS_LEN {
        return Err(PduError::ShortHeader);
    }
    let h = &buf[..BHS_LEN];
    let op = h[0] & 0x3f;
    if h[4] != 0 {
        return Err(PduError::UnexpectedAhs);
    }
    let data_len = be24(h, 5);
    if data_len > MAX_DATA_SEGMENT {
        return Err(PduError::DataSegmentTooLong);
    }
    let total = BHS_LEN + data_len + pad4(data_len);
    if buf.len() < total {
        return Err(PduError::ShortData);
    }
    let data = &buf[BHS_LEN..BHS_LEN + data_len];

    let req = match op {
        opcode::LOGIN_REQ => Request::Login(LoginRequest {
            transit: h[1] & 0x80 != 0,
            continue_: h[1] & 0x40 != 0,
            csg: (h[1] >> 2) & 0x3,
            nsg: h[1] & 0x3,
            version_max: h[2],
            version_min: h[3],
            isid: [h[8], h[9], h[10], h[11], h[12], h[13]],
            tsih: be16(h, 14),
            itt: be32(h, 16),
            cid: be16(h, 20),
            cmd_sn: be32(h, 24),
            exp_stat_sn: be32(h, 28),
            text: text::parse_pairs(data),
        }),
        opcode::TEXT_REQ => Request::Text(TextRequest {
            final_: h[1] & 0x80 != 0,
            continue_: h[1] & 0x40 != 0,
            lun: be64(h, 8),
            itt: be32(h, 16),
            ttt: be32(h, 20),
            cmd_sn: be32(h, 24),
            exp_stat_sn: be32(h, 28),
            text: text::parse_pairs(data),
        }),
        opcode::SCSI_COMMAND => {
            let mut cdb = [0u8; 16];
            cdb.copy_from_slice(&h[32..48]);
            Request::ScsiCommand(ScsiCommand {
                final_: h[1] & 0x80 != 0,
                read: h[1] & 0x40 != 0,
                write: h[1] & 0x20 != 0,
                attr: h[1] & 0x07,
                lun: be64(h, 8),
                itt: be32(h, 16),
                edtl: be32(h, 20),
                cmd_sn: be32(h, 24),
                exp_stat_sn: be32(h, 28),
                cdb,
                data: data.to_vec(),
            })
        }
        opcode::DATA_OUT => Request::DataOut(ScsiDataOut {
            final_: h[1] & 0x80 != 0,
            lun: be64(h, 8),
            itt: be32(h, 16),
            ttt: be32(h, 20),
            exp_stat_sn: be32(h, 28),
            data_sn: be32(h, 36),
            buffer_offset: be32(h, 40),
            data: data.to_vec(),
        }),
        opcode::NOP_OUT => Request::NopOut(NopOut {
            lun: be64(h, 8),
            itt: be32(h, 16),
            ttt: be32(h, 20),
            cmd_sn: be32(h, 24),
            exp_stat_sn: be32(h, 28),
            data: data.to_vec(),
        }),
        opcode::LOGOUT_REQ => Request::Logout(LogoutRequest {
            reason: h[1] & 0x7f,
            itt: be32(h, 16),
            cid: be16(h, 20),
            cmd_sn: be32(h, 24),
            exp_stat_sn: be32(h, 28),
        }),
        opcode::TASK_MGMT => Request::TaskMgmt(TaskMgmt {
            function: h[1] & 0x7f,
            lun: be64(h, 8),
            itt: be32(h, 16),
            ref_task_tag: be32(h, 20),
            cmd_sn: be32(h, 24),
            exp_stat_sn: be32(h, 28),
        }),
        other => Request::Unsupported {
            opcode: other,
            itt: be32(h, 16),
        },
    };
    Ok((req, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::{opcode, PduError, BHS_LEN};

    /// Build a 48-byte BHS with the given opcode in byte 0; all else zero.
    fn bhs(op: u8) -> [u8; BHS_LEN] {
        let mut b = [0u8; BHS_LEN];
        b[0] = op;
        b
    }

    #[test]
    fn short_buffer_is_short_header() {
        assert_eq!(decode(&[0u8; 10]), Err(PduError::ShortHeader));
    }

    #[test]
    fn nonzero_ahs_rejected() {
        let mut b = bhs(opcode::NOP_OUT);
        b[4] = 1; // TotalAHSLength
        assert_eq!(decode(&b), Err(PduError::UnexpectedAhs));
    }

    #[test]
    fn data_segment_length_capped() {
        let mut b = bhs(opcode::SCSI_COMMAND);
        // DataSegmentLength = 0xFFFFFF (> 256 KiB)
        b[5] = 0xFF;
        b[6] = 0xFF;
        b[7] = 0xFF;
        assert_eq!(decode(&b), Err(PduError::DataSegmentTooLong));
    }

    #[test]
    fn declared_data_past_end_is_short_data() {
        let mut b = bhs(opcode::TEXT_REQ).to_vec();
        b[7] = 8; // DataSegmentLength = 8, but no data bytes follow
        assert_eq!(decode(&b), Err(PduError::ShortData));
    }

    #[test]
    fn unknown_opcode_becomes_unsupported() {
        let mut b = bhs(0x1a); // not a known opcode
        b[16..20].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // ITT
        let (req, consumed) = decode(&b).unwrap();
        assert_eq!(consumed, BHS_LEN);
        assert_eq!(
            req,
            Request::Unsupported {
                opcode: 0x1a,
                itt: 0xDEAD_BEEF
            }
        );
    }

    #[test]
    fn decodes_login_request_fields_and_text() {
        let mut b = bhs(opcode::LOGIN_REQ).to_vec();
        b[1] = 0x87; // T=1, C=0, CSG=1 (bits 3..2 = 01), NSG=3 (bits 1..0 = 11)
        b[2] = 0x00; // version_max
        b[3] = 0x00; // version_min
        b[8..14].copy_from_slice(&[1, 2, 3, 4, 5, 6]); // ISID
        b[14..16].copy_from_slice(&0u16.to_be_bytes()); // TSIH
        b[16..20].copy_from_slice(&0x11u32.to_be_bytes()); // ITT
        b[20..22].copy_from_slice(&0u16.to_be_bytes()); // CID
        b[24..28].copy_from_slice(&5u32.to_be_bytes()); // CmdSN
        b[28..32].copy_from_slice(&7u32.to_be_bytes()); // ExpStatSN
        let text = b"AuthMethod=None\0";
        b[5..8].copy_from_slice(&[0, 0, text.len() as u8]); // DataSegmentLength
        b.extend_from_slice(text);
        // pad to 4-byte boundary (len 16 already multiple of 4 -> no pad)

        let (req, consumed) = decode(&b).unwrap();
        assert_eq!(consumed, BHS_LEN + 16);
        let Request::Login(l) = req else {
            panic!("not a login")
        };
        assert!(l.transit);
        assert!(!l.continue_);
        assert_eq!(l.csg, 1);
        assert_eq!(l.nsg, 3);
        assert_eq!(l.isid, [1, 2, 3, 4, 5, 6]);
        assert_eq!(l.itt, 0x11);
        assert_eq!(l.cmd_sn, 5);
        assert_eq!(l.exp_stat_sn, 7);
        assert_eq!(l.text, vec![("AuthMethod".to_string(), "None".to_string())]);
    }

    #[test]
    fn decodes_scsi_read10_command() {
        let mut b = bhs(opcode::SCSI_COMMAND);
        b[1] = 0xC2; // F=1 (0x80), R=1 (0x40), W=0, ATTR=2 (SIMPLE)
        b[8..16].copy_from_slice(&1u64.to_be_bytes()); // LUN = 1
        b[16..20].copy_from_slice(&0x42u32.to_be_bytes()); // ITT
        b[20..24].copy_from_slice(&512u32.to_be_bytes()); // EDTL
        b[24..28].copy_from_slice(&9u32.to_be_bytes()); // CmdSN
                                                        // CDB starts at BHS byte 32; READ(10) opcode is 0x28. Only cdb[0] is
                                                        // asserted here — LBA/length interpretation is 05b's job.
        b[32] = 0x28;
        let (req, _) = decode(&b).unwrap();
        let Request::ScsiCommand(c) = req else {
            panic!("not scsi")
        };
        assert!(c.final_);
        assert!(c.read);
        assert!(!c.write);
        assert_eq!(c.attr, 2);
        assert_eq!(c.lun, 1);
        assert_eq!(c.itt, 0x42);
        assert_eq!(c.edtl, 512);
        assert_eq!(c.cmd_sn, 9);
        assert_eq!(c.cdb[0], 0x28);
    }

    #[test]
    fn decodes_data_out_payload() {
        let mut b = bhs(opcode::DATA_OUT).to_vec();
        b[1] = 0x80; // F=1
        b[16..20].copy_from_slice(&0x42u32.to_be_bytes()); // ITT
        b[20..24].copy_from_slice(&0x99u32.to_be_bytes()); // TTT
        b[36..40].copy_from_slice(&0u32.to_be_bytes()); // DataSN
        b[40..44].copy_from_slice(&0u32.to_be_bytes()); // BufferOffset
        let payload = [0xAB, 0xCD, 0xEF]; // 3 bytes -> 1 byte padding
        b[5..8].copy_from_slice(&[0, 0, payload.len() as u8]);
        b.extend_from_slice(&payload);
        b.push(0); // padding to 4 bytes

        let (req, consumed) = decode(&b).unwrap();
        assert_eq!(consumed, BHS_LEN + 4); // 3 bytes + 1 pad
        let Request::DataOut(d) = req else {
            panic!("not data-out")
        };
        assert!(d.final_);
        assert_eq!(d.itt, 0x42);
        assert_eq!(d.ttt, 0x99);
        assert_eq!(d.data, payload);
    }

    #[test]
    fn decodes_logout_reason() {
        let mut b = bhs(opcode::LOGOUT_REQ);
        b[1] = 0x80 | 0x01; // F + reason code 1 (close connection)
        b[16..20].copy_from_slice(&0x7u32.to_be_bytes()); // ITT
        b[20..22].copy_from_slice(&3u16.to_be_bytes()); // CID
        let (req, _) = decode(&b).unwrap();
        let Request::Logout(l) = req else {
            panic!("not logout")
        };
        assert_eq!(l.reason, 1);
        assert_eq!(l.itt, 7);
        assert_eq!(l.cid, 3);
    }
}
