//! iSCSI wire protocol — PDU codec (phase 05a).
//!
//! Pure, synchronous bytes <-> typed structs. No I/O, no session state. Session
//! sequencing lives in phase 05c; SCSI CDB interpretation in phase 05b.

mod pdu;
pub mod registry;
pub mod scsi;
pub mod session;
pub mod text;

pub use pdu::{
    decode, LoginRequest, LoginResponse, LogoutRequest, LogoutResponse, NopIn, NopOut, R2t, Reject,
    Request, ScsiCommand, ScsiDataIn, ScsiDataOut, ScsiResponse, TaskMgmt, TaskMgmtResponse,
    TextRequest, TextResponse,
};

use thiserror::Error;

/// Fixed size of the iSCSI Basic Header Segment.
pub const BHS_LEN: usize = 48;

/// Defensive cap on a single PDU's data segment length. The *negotiated*
/// MaxRecvDataSegmentLength is a phase-05c concern; this is only a sanity bound
/// so a hostile length field can't trigger a huge allocation. 256 KiB.
pub const MAX_DATA_SEGMENT: usize = 256 * 1024;

/// iSCSI opcodes (low 6 bits of BHS byte 0).
pub mod opcode {
    // initiator -> target
    pub const NOP_OUT: u8 = 0x00;
    pub const SCSI_COMMAND: u8 = 0x01;
    pub const TASK_MGMT: u8 = 0x02;
    pub const LOGIN_REQ: u8 = 0x03;
    pub const TEXT_REQ: u8 = 0x04;
    pub const DATA_OUT: u8 = 0x05;
    pub const LOGOUT_REQ: u8 = 0x06;
    // target -> initiator
    pub const NOP_IN: u8 = 0x20;
    pub const SCSI_RESP: u8 = 0x21;
    pub const TASK_MGMT_RESP: u8 = 0x22;
    pub const LOGIN_RESP: u8 = 0x23;
    pub const TEXT_RESP: u8 = 0x24;
    pub const DATA_IN: u8 = 0x25;
    pub const LOGOUT_RESP: u8 = 0x26;
    pub const R2T: u8 = 0x31;
    pub const REJECT: u8 = 0x3f;
}

/// Errors from decoding a PDU. Structural corruption only — an unknown *opcode*
/// is not an error (it becomes `Request::Unsupported`).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PduError {
    #[error("buffer shorter than the 48-byte BHS")]
    ShortHeader,
    #[error("declared data segment runs past the end of the buffer")]
    ShortData,
    #[error("AHS present but unsupported (TotalAHSLength != 0)")]
    UnexpectedAhs,
    #[error("data segment length exceeds MAX_DATA_SEGMENT")]
    DataSegmentTooLong,
}

/// Padding needed to round `n` bytes up to a 4-byte boundary.
pub(crate) fn pad4(n: usize) -> usize {
    (4 - (n % 4)) % 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad4_rounds_up_to_multiple_of_four() {
        assert_eq!(pad4(0), 0);
        assert_eq!(pad4(1), 3);
        assert_eq!(pad4(2), 2);
        assert_eq!(pad4(3), 1);
        assert_eq!(pad4(4), 0);
        assert_eq!(pad4(5), 3);
    }

    #[test]
    fn opcodes_have_expected_values() {
        assert_eq!(opcode::SCSI_COMMAND, 0x01);
        assert_eq!(opcode::LOGIN_REQ, 0x03);
        assert_eq!(opcode::DATA_OUT, 0x05);
        assert_eq!(opcode::REJECT, 0x3f);
    }
}
