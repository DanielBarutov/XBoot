# Phase 05a — iSCSI PDU Codec Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A pure, synchronous iSCSI PDU codec in `xboot-core` — bytes ↔ typed request/response structs — with no I/O, no session state, and a `decode` that never panics on arbitrary input.

**Architecture:** New module `xboot-core/src/iscsi/` with `mod.rs` (constants, opcodes, `PduError`, padding), `text.rs` (Key=Value segments), and `pdu.rs` (`decode` → `Request` enum; response builders → bytes). Sans-I/O: the codec only transforms bytes; sequencing (05c) and SCSI CDB interpretation (05b) live elsewhere. All multi-byte fields big-endian.

**Tech Stack:** Rust, `thiserror` (already a dep), `proptest` (dev-dep), `cargo-fuzz` (existing `fuzz/` crate). No new runtime dependencies.

**Spec:** `docs/superpowers/specs/2026-06-03-phase05a-iscsi-pdu-design.md`

---

## File Structure

- Create: `crates/xboot-core/src/iscsi/mod.rs` — module root: `BHS_LEN`, `MAX_DATA_SEGMENT`, `opcode` submodule, `PduError`, `pad4`, re-exports.
- Create: `crates/xboot-core/src/iscsi/text.rs` — `parse_pairs` / `encode_pairs`.
- Create: `crates/xboot-core/src/iscsi/pdu.rs` — `Request` enum + request structs + `decode`; response builder structs + `encode`.
- Modify: `crates/xboot-core/src/lib.rs` — add `pub mod iscsi;`.
- Modify: `fuzz/Cargo.toml` — add `iscsi_pdu` bin target.
- Create: `fuzz/fuzz_targets/iscsi_pdu.rs` — fuzz `decode`.

All BHS byte offsets below follow RFC 7143. The codec treats the BHS as exactly 48 bytes; every reader offset is a fixed constant ≤ 44, so byte access never panics regardless of input.

---

## Task 1: Module scaffold (constants, opcodes, error, padding)

**Files:**
- Modify: `crates/xboot-core/src/lib.rs`
- Create: `crates/xboot-core/src/iscsi/mod.rs`

This task is a pure-constants scaffold (no behavior to red-green), so we create the module with its content and tests together, then verify the tests pass. `pdu.rs` is filled in Task 3; for now its `mod`/`pub use` lines stay commented out so the crate compiles.

- [ ] **Step 1: Create `mod.rs` with constants, error type, and tests**

Create `crates/xboot-core/src/iscsi/mod.rs`:

```rust
//! iSCSI wire protocol — PDU codec (phase 05a).
//!
//! Pure, synchronous bytes <-> typed structs. No I/O, no session state. Session
//! sequencing lives in phase 05c; SCSI CDB interpretation in phase 05b.

// mod pdu;            // re-enabled in Task 3
pub mod text;
// pub use pdu::{ ... }; // re-enabled in Task 3

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
// `#[allow(dead_code)]` is needed only in Task 1, where no non-test code uses
// `pad4` yet. Remove this attribute in Task 3, where `decode` calls it.
#[allow(dead_code)]
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
```

Add the module declaration to `crates/xboot-core/src/lib.rs`. The file currently reads:

```rust
pub mod cache;
pub mod config;
pub mod net;
pub mod storage;
pub mod volume;
```

Change it to (keep alphabetical order):

```rust
pub mod cache;
pub mod config;
pub mod iscsi;
pub mod net;
pub mod storage;
pub mod volume;
```

`mod.rs` declares `pub mod text;` but not `mod pdu;` (commented out), so the crate compiles once `text.rs` exists as a stub.

- [ ] **Step 2: Create the `text.rs` stub (filled in Task 2)**

Create `crates/xboot-core/src/iscsi/text.rs`:

```rust
// Filled in by Task 2.
```

- [ ] **Step 3: Register the module in `lib.rs`**

`crates/xboot-core/src/lib.rs` currently reads:

```rust
pub mod cache;
pub mod config;
pub mod net;
pub mod storage;
pub mod volume;
```

Change it to (keep alphabetical order):

```rust
pub mod cache;
pub mod config;
pub mod iscsi;
pub mod net;
pub mod storage;
pub mod volume;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p xboot-core iscsi::tests`
Expected: PASS (`pad4_rounds_up_to_multiple_of_four`, `opcodes_have_expected_values`).

- [ ] **Step 5: Lint**

Run: `cargo clippy -p xboot-core --all-targets`
Expected: no warnings (the `#[allow(dead_code)]` on `pad4` keeps the lib build clean).

- [ ] **Step 6: Commit**

```bash
git add crates/xboot-core/src/lib.rs crates/xboot-core/src/iscsi/mod.rs crates/xboot-core/src/iscsi/text.rs
git commit -m "feat(iscsi): module scaffold — opcodes, PduError, padding"
```

---

## Task 2: Text segment codec (`text.rs`)

**Files:**
- Modify: `crates/xboot-core/src/iscsi/text.rs`

- [ ] **Step 1: Write the failing tests**

Replace `crates/xboot-core/src/iscsi/text.rs` test stub with these tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nul_separated_pairs() {
        let data = b"AuthMethod=None\0MaxRecvDataSegmentLength=8192\0";
        let pairs = parse_pairs(data);
        assert_eq!(
            pairs,
            vec![
                ("AuthMethod".to_string(), "None".to_string()),
                ("MaxRecvDataSegmentLength".to_string(), "8192".to_string()),
            ]
        );
    }

    #[test]
    fn value_may_contain_equals_sign() {
        // Only the first '=' separates key from value.
        let pairs = parse_pairs(b"TargetAddress=10.0.0.1:3260,1\0");
        assert_eq!(pairs, vec![("TargetAddress".into(), "10.0.0.1:3260,1".into())]);
    }

    #[test]
    fn skips_malformed_entries() {
        // no '=' -> skipped; empty key -> skipped; trailing empty chunk -> skipped
        let pairs = parse_pairs(b"novalue\0=onlyvalue\0Good=1\0");
        assert_eq!(pairs, vec![("Good".into(), "1".into())]);
    }

    #[test]
    fn encode_appends_nul_after_each_pair() {
        let out = encode_pairs(&[("A".into(), "1".into()), ("B".into(), "2".into())]);
        assert_eq!(out, b"A=1\0B=2\0");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xboot-core iscsi::text`
Expected: FAIL — `parse_pairs` / `encode_pairs` not found.

- [ ] **Step 3: Implement**

Put above the test module in `text.rs`:

```rust
//! iSCSI text segments: `Key=Value\0Key=Value\0...`.

/// Parse a text segment into key/value pairs. Splitting is on the *first* `=`.
/// Malformed entries (no `=`, empty key) and empty chunks are skipped. Never
/// panics; non-UTF-8 bytes are replaced (lossy).
pub fn parse_pairs(data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in data.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let Some(eq) = chunk.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = &chunk[..eq];
        if key.is_empty() {
            continue;
        }
        let val = &chunk[eq + 1..];
        out.push((
            String::from_utf8_lossy(key).into_owned(),
            String::from_utf8_lossy(val).into_owned(),
        ));
    }
    out
}

/// Encode pairs into a text segment: each `Key=Value` followed by a NUL.
pub fn encode_pairs(pairs: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in pairs {
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
        out.push(0);
    }
    out
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xboot-core iscsi::text`
Expected: PASS (4 tests).

- [ ] **Step 5: Add the round-trip property test**

Append to the `tests` module in `text.rs`:

```rust
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn pairs_round_trip(
            // keys: non-empty, no '=' and no NUL; values: no NUL.
            pairs in proptest::collection::vec(
                ("[A-Za-z][A-Za-z0-9]{0,15}", "[ -<>-~]{0,32}"),
                0..16,
            )
        ) {
            let pairs: Vec<(String, String)> = pairs.into_iter().collect();
            let encoded = encode_pairs(&pairs);
            let decoded = parse_pairs(&encoded);
            prop_assert_eq!(decoded, pairs);
        }
    }
```

Note: round-trip requires only that keys contain no `=` and that neither key nor value contains NUL. The key regex `[A-Za-z][A-Za-z0-9]{0,15}` guarantees the no-`=`/no-NUL key property; the value regex `[ -<>-~]{0,32}` is printable ASCII without NUL (it also happens to exclude `=`, which is fine — the `value_may_contain_equals_sign` unit test already covers `=` inside values).

- [ ] **Step 6: Run the property test**

Run: `cargo test -p xboot-core iscsi::text`
Expected: PASS (5 tests including `pairs_round_trip`).

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/iscsi/text.rs
git commit -m "feat(iscsi): text segment codec with round-trip proptest"
```

---

## Task 3: Decode — request structs, byte readers, framing, all opcodes

**Files:**
- Modify: `crates/xboot-core/src/iscsi/mod.rs` (re-enable `mod pdu;` and the `pub use`)
- Create/replace: `crates/xboot-core/src/iscsi/pdu.rs`

- [ ] **Step 1: Re-enable the pdu module**

In `crates/xboot-core/src/iscsi/mod.rs`, uncomment `mod pdu;` and add a `pub use`
that re-exports **only the request-side items defined in this task**. The
response builders are added to this list in Task 4. The top should read:

```rust
//! iSCSI wire protocol — PDU codec (phase 05a).

mod pdu;
pub mod text;

pub use pdu::{
    decode, LoginRequest, LogoutRequest, NopOut, Request, ScsiCommand, ScsiDataOut, TaskMgmt,
    TextRequest,
};
```

- [ ] **Step 2: Write the failing tests**

Create `crates/xboot-core/src/iscsi/pdu.rs` with this test module at the bottom (impl comes in step 4). These tests build BHS byte arrays by hand and assert decoded fields — the real decode path.

```rust
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
        assert_eq!(req, Request::Unsupported { opcode: 0x1a, itt: 0xDEAD_BEEF });
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
        let Request::Login(l) = req else { panic!("not a login") };
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
        let Request::ScsiCommand(c) = req else { panic!("not scsi") };
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
        let Request::DataOut(d) = req else { panic!("not data-out") };
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
        let Request::Logout(l) = req else { panic!("not logout") };
        assert_eq!(l.reason, 1);
        assert_eq!(l.itt, 7);
        assert_eq!(l.cid, 3);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p xboot-core iscsi::pdu`
Expected: FAIL — `decode`, `Request`, etc. not found.

- [ ] **Step 4: Implement `pdu.rs`**

Put this ABOVE the test module in `crates/xboot-core/src/iscsi/pdu.rs`:

```rust
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
        b[off], b[off + 1], b[off + 2], b[off + 3], b[off + 4], b[off + 5], b[off + 6], b[off + 7],
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
    Unsupported { opcode: u8, itt: u32 },
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p xboot-core iscsi::pdu`
Expected: PASS (all decode tests).

- [ ] **Step 6: Remove the temporary `pad4` allow, then lint and format**

`decode` now calls `pad4`, so the Task 1 workaround is no longer needed. In `crates/xboot-core/src/iscsi/mod.rs`, delete the two-line comment and the `#[allow(dead_code)]` attribute above `pub(crate) fn pad4`, leaving just the doc comment and the function.

Run: `cargo clippy -p xboot-core --all-targets && cargo fmt`
Expected: no warnings (`be64`/`be16`/`pad4` are all used now).

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/iscsi/mod.rs crates/xboot-core/src/iscsi/pdu.rs
git commit -m "feat(iscsi): decode all request opcodes with framing and error paths"
```

---

## Task 4: Encode — response builders

**Files:**
- Modify: `crates/xboot-core/src/iscsi/mod.rs` (extend the `pub use` with response types)
- Modify: `crates/xboot-core/src/iscsi/pdu.rs`

- [ ] **Step 1: Extend the re-export with the response builders**

In `crates/xboot-core/src/iscsi/mod.rs`, replace the request-only `pub use pdu::{...}`
from Task 3 with the full list (request types + the response builders this task adds):

```rust
pub use pdu::{
    decode, LoginRequest, LoginResponse, LogoutRequest, LogoutResponse, NopIn, NopOut, R2t,
    Reject, Request, ScsiCommand, ScsiDataIn, ScsiDataOut, ScsiResponse, TaskMgmt, TextRequest,
    TextResponse,
};
```

(The crate will not compile until Step 3 defines these types — that's expected within this task; run the build at Step 4.)

- [ ] **Step 2: Write the failing tests**

Append to the `tests` module in `pdu.rs`:

```rust
    #[test]
    fn login_response_encodes_header_and_text() {
        let resp = LoginResponse {
            transit: true,
            continue_: false,
            csg: 1,
            nsg: 3,
            version_max: 0,
            version_active: 0,
            isid: [1, 2, 3, 4, 5, 6],
            tsih: 0x1234,
            itt: 0x42,
            stat_sn: 1,
            exp_cmd_sn: 6,
            max_cmd_sn: 10,
            status_class: 0,
            status_detail: 0,
            text: vec![("HeaderDigest".into(), "None".into())],
        };
        let bytes = resp.encode();
        assert_eq!(bytes[0] & 0x3f, opcode::LOGIN_RESP);
        assert_eq!(bytes[1] & 0x80, 0x80); // transit
        assert_eq!(bytes[4], 0); // TotalAHSLength
        let text = b"HeaderDigest=None\0"; // len 18 -> pad to 20
        let dsl = ((bytes[5] as usize) << 16) | ((bytes[6] as usize) << 8) | bytes[7] as usize;
        assert_eq!(dsl, text.len());
        assert_eq!(&bytes[48..48 + text.len()], text);
        assert_eq!(bytes.len(), 48 + 20); // padded to 4-byte boundary
        assert_eq!(bytes[14..16], 0x1234u16.to_be_bytes()); // TSIH
        assert_eq!(bytes[16..20], 0x42u32.to_be_bytes()); // ITT
    }

    #[test]
    fn scsi_response_good_status_no_data() {
        let resp = ScsiResponse {
            response: 0x00,
            status: 0x00, // GOOD
            itt: 0x42,
            stat_sn: 5,
            exp_cmd_sn: 6,
            max_cmd_sn: 10,
            residual: 0,
            sense: Vec::new(),
        };
        let bytes = resp.encode();
        assert_eq!(bytes[0] & 0x3f, opcode::SCSI_RESP);
        assert_eq!(bytes[1] & 0x80, 0x80); // F always set on SCSI Response
        assert_eq!(bytes[2], 0x00); // response
        assert_eq!(bytes[3], 0x00); // status GOOD
        assert_eq!(bytes[16..20], 0x42u32.to_be_bytes());
        assert_eq!(bytes.len(), 48); // no data
    }

    #[test]
    fn scsi_data_in_carries_payload_and_offset() {
        let resp = ScsiDataIn {
            final_: true,
            ack: false,
            has_status: true,
            status: 0x00,
            lun: 1,
            itt: 0x42,
            ttt: 0xffff_ffff,
            stat_sn: 5,
            exp_cmd_sn: 6,
            max_cmd_sn: 10,
            data_sn: 0,
            buffer_offset: 512,
            data: vec![0xAA, 0xBB, 0xCC, 0xDD],
        };
        let bytes = resp.encode();
        assert_eq!(bytes[0] & 0x3f, opcode::DATA_IN);
        assert_eq!(bytes[1] & 0x80, 0x80); // F
        assert_eq!(bytes[1] & 0x01, 0x01); // S (status present)
        assert_eq!(bytes[40..44], 512u32.to_be_bytes()); // BufferOffset
        assert_eq!(&bytes[48..52], &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(bytes.len(), 52);
    }

    #[test]
    fn r2t_encodes_transfer_request() {
        let r2t = R2t {
            lun: 1,
            itt: 0x42,
            ttt: 0x77,
            stat_sn: 5,
            exp_cmd_sn: 6,
            max_cmd_sn: 10,
            r2t_sn: 0,
            buffer_offset: 0,
            desired_length: 4096,
        };
        let bytes = r2t.encode();
        assert_eq!(bytes[0] & 0x3f, opcode::R2T);
        assert_eq!(bytes[20..24], 0x77u32.to_be_bytes()); // TTT
        assert_eq!(bytes[44..48], 4096u32.to_be_bytes()); // DesiredDataTransferLength
        assert_eq!(bytes.len(), 48);
    }

    #[test]
    fn nop_in_and_logout_and_text_and_reject_encode_opcodes() {
        let nop = NopIn { lun: 0, itt: 0xffff_ffff, ttt: 0xffff_ffff, stat_sn: 1, exp_cmd_sn: 2, max_cmd_sn: 3, data: vec![1, 2, 3] };
        assert_eq!(nop.encode()[0] & 0x3f, opcode::NOP_IN);
        assert_eq!(nop.encode().len(), 48 + 4); // 3 bytes + 1 pad

        let lo = LogoutResponse { response: 0, stat_sn: 1, exp_cmd_sn: 2, max_cmd_sn: 3 };
        let lob = lo.encode();
        assert_eq!(lob[0] & 0x3f, opcode::LOGOUT_RESP);
        assert_eq!(lob[2], 0); // response
        assert_eq!(lob.len(), 48);

        let tr = TextResponse { final_: true, continue_: false, itt: 0x42, ttt: 0xffff_ffff, stat_sn: 1, exp_cmd_sn: 2, max_cmd_sn: 3, text: vec![("TargetName".into(), "iqn.x".into())] };
        let trb = tr.encode();
        assert_eq!(trb[0] & 0x3f, opcode::TEXT_RESP);
        assert_eq!(trb[1] & 0x80, 0x80); // F

        let rj = Reject { reason: 0x05, stat_sn: 1, exp_cmd_sn: 2, max_cmd_sn: 3, header: vec![0u8; 48] };
        let rjb = rj.encode();
        assert_eq!(rjb[0] & 0x3f, opcode::REJECT);
        assert_eq!(rjb[2], 0x05); // reason
        assert_eq!(rjb.len(), 48 + 48); // rejected header echoed as data
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p xboot-core iscsi::pdu`
Expected: FAIL — response builder types not found.

- [ ] **Step 4: Implement the builders**

Add to `pdu.rs` (above the test module, after the request code). First, the shared writers and framing helper:

```rust
// ---- big-endian writers into a mutable BHS ----

fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_be_bytes());
}
fn put24(b: &mut [u8], off: usize, v: usize) {
    b[off] = (v >> 16) as u8;
    b[off + 1] = (v >> 8) as u8;
    b[off + 2] = v as u8;
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

/// Assemble a PDU from a fully-populated 48-byte BHS plus a data segment.
/// Writes DataSegmentLength into bytes 5..8 and pads the data to 4 bytes.
fn frame(mut bhs: [u8; BHS_LEN], data: &[u8]) -> Vec<u8> {
    put24(&mut bhs, 5, data.len());
    let mut out = Vec::with_capacity(BHS_LEN + data.len() + pad4(data.len()));
    out.extend_from_slice(&bhs);
    out.extend_from_slice(data);
    out.resize(out.len() + pad4(data.len()), 0);
    out
}

// ---- response types (target -> initiator) ----

#[derive(Debug, Clone)]
pub struct LoginResponse {
    pub transit: bool,
    pub continue_: bool,
    pub csg: u8,
    pub nsg: u8,
    pub version_max: u8,
    pub version_active: u8,
    pub isid: [u8; 6],
    pub tsih: u16,
    pub itt: u32,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
    pub status_class: u8,
    pub status_detail: u8,
    pub text: Vec<(String, String)>,
}

impl LoginResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::LOGIN_RESP;
        if self.transit {
            h[1] |= 0x80;
        }
        if self.continue_ {
            h[1] |= 0x40;
        }
        h[1] |= (self.csg & 0x3) << 2;
        h[1] |= self.nsg & 0x3;
        h[2] = self.version_max;
        h[3] = self.version_active;
        h[8..14].copy_from_slice(&self.isid);
        put16(&mut h, 14, self.tsih);
        put32(&mut h, 16, self.itt);
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        h[36] = self.status_class;
        h[37] = self.status_detail;
        frame(h, &text::encode_pairs(&self.text))
    }
}

#[derive(Debug, Clone)]
pub struct TextResponse {
    pub final_: bool,
    pub continue_: bool,
    pub itt: u32,
    pub ttt: u32,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
    pub text: Vec<(String, String)>,
}

impl TextResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::TEXT_RESP;
        if self.final_ {
            h[1] |= 0x80;
        }
        if self.continue_ {
            h[1] |= 0x40;
        }
        put32(&mut h, 16, self.itt);
        put32(&mut h, 20, self.ttt);
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        frame(h, &text::encode_pairs(&self.text))
    }
}

#[derive(Debug, Clone)]
pub struct ScsiResponse {
    pub response: u8,
    pub status: u8,
    pub itt: u32,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
    pub residual: u32,
    pub sense: Vec<u8>,
}

impl ScsiResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::SCSI_RESP;
        h[1] = 0x80; // F is always set on a SCSI Response
        h[2] = self.response;
        h[3] = self.status;
        put32(&mut h, 16, self.itt);
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        put32(&mut h, 44, self.residual);
        frame(h, &self.sense)
    }
}

#[derive(Debug, Clone)]
pub struct ScsiDataIn {
    pub final_: bool,
    pub ack: bool,
    pub has_status: bool,
    pub status: u8,
    pub lun: u64,
    pub itt: u32,
    pub ttt: u32,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
    pub data_sn: u32,
    pub buffer_offset: u32,
    pub data: Vec<u8>,
}

impl ScsiDataIn {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::DATA_IN;
        if self.final_ {
            h[1] |= 0x80;
        }
        if self.ack {
            h[1] |= 0x40;
        }
        if self.has_status {
            h[1] |= 0x01; // S bit; status valid only with F set
        }
        h[3] = self.status;
        put64(&mut h, 8, self.lun);
        put32(&mut h, 16, self.itt);
        put32(&mut h, 20, self.ttt);
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        put32(&mut h, 36, self.data_sn);
        put32(&mut h, 40, self.buffer_offset);
        frame(h, &self.data)
    }
}

#[derive(Debug, Clone)]
pub struct R2t {
    pub lun: u64,
    pub itt: u32,
    pub ttt: u32,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
    pub r2t_sn: u32,
    pub buffer_offset: u32,
    pub desired_length: u32,
}

impl R2t {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::R2T;
        h[1] = 0x80; // F always set
        put64(&mut h, 8, self.lun);
        put32(&mut h, 16, self.itt);
        put32(&mut h, 20, self.ttt);
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        put32(&mut h, 36, self.r2t_sn);
        put32(&mut h, 40, self.buffer_offset);
        put32(&mut h, 44, self.desired_length);
        frame(h, &[])
    }
}

#[derive(Debug, Clone)]
pub struct NopIn {
    pub lun: u64,
    pub itt: u32,
    pub ttt: u32,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
    pub data: Vec<u8>,
}

impl NopIn {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::NOP_IN;
        h[1] = 0x80; // F always set
        put64(&mut h, 8, self.lun);
        put32(&mut h, 16, self.itt);
        put32(&mut h, 20, self.ttt);
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        frame(h, &self.data)
    }
}

#[derive(Debug, Clone)]
pub struct LogoutResponse {
    pub response: u8,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
}

impl LogoutResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::LOGOUT_RESP;
        h[1] = 0x80; // F always set
        h[2] = self.response;
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        frame(h, &[])
    }
}

#[derive(Debug, Clone)]
pub struct Reject {
    pub reason: u8,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
    /// The 48-byte BHS of the rejected PDU, echoed back as the data segment.
    pub header: Vec<u8>,
}

impl Reject {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::REJECT;
        h[1] = 0x80; // F always set
        h[2] = self.reason;
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        frame(h, &self.header)
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p xboot-core iscsi::pdu`
Expected: PASS (decode tests + the 5 new encode tests).

- [ ] **Step 6: Lint and format**

Run: `cargo clippy -p xboot-core --all-targets && cargo fmt`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/iscsi/mod.rs crates/xboot-core/src/iscsi/pdu.rs
git commit -m "feat(iscsi): response builders (login/scsi/data-in/r2t/nop/logout/text/reject)"
```

---

## Task 5: Property tests — decode robustness and encode invariant

**Files:**
- Modify: `crates/xboot-core/src/iscsi/pdu.rs`

- [ ] **Step 1: Write the property tests**

Append a `prop` module at the very bottom of `pdu.rs` (after the `tests` module):

```rust
#[cfg(test)]
mod prop {
    use super::*;
    use crate::iscsi::{BHS_LEN, MAX_DATA_SEGMENT};
    use proptest::prelude::*;

    proptest! {
        // decode never panics on arbitrary input.
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let _ = decode(&bytes);
        }

        // When decode succeeds, `consumed` is within the buffer and 4-byte aligned.
        #[test]
        fn decode_consumed_is_consistent(bytes in proptest::collection::vec(any::<u8>(), 48..4096)) {
            if let Ok((_, consumed)) = decode(&bytes) {
                prop_assert!(consumed <= bytes.len());
                prop_assert_eq!(consumed % 4, 0);
                prop_assert!(consumed >= BHS_LEN);
            }
        }

        // Every encoded response has a 4-aligned length, zero AHS, and a
        // DataSegmentLength field matching the payload it carries.
        #[test]
        fn scsi_data_in_encode_invariants(
            payload in proptest::collection::vec(any::<u8>(), 0..1024),
            itt in any::<u32>(),
            offset in any::<u32>(),
        ) {
            let n = payload.len();
            let bytes = ScsiDataIn {
                final_: true, ack: false, has_status: false, status: 0,
                lun: 0, itt, ttt: 0xffff_ffff, stat_sn: 0, exp_cmd_sn: 0,
                max_cmd_sn: 0, data_sn: 0, buffer_offset: offset, data: payload,
            }.encode();

            prop_assert_eq!(bytes.len() % 4, 0);
            prop_assert!(bytes.len() <= BHS_LEN + n + 3);
            prop_assert_eq!(bytes[4], 0); // TotalAHSLength
            let dsl = ((bytes[5] as usize) << 16) | ((bytes[6] as usize) << 8) | bytes[7] as usize;
            prop_assert_eq!(dsl, n);
            prop_assert!(n <= MAX_DATA_SEGMENT);
        }
    }
}
```

- [ ] **Step 2: Run the property tests**

Run: `cargo test -p xboot-core iscsi::pdu::prop`
Expected: PASS (3 property tests).

- [ ] **Step 3: Run with more cases to stress it**

Run: `PROPTEST_CASES=4096 cargo test -p xboot-core iscsi::pdu::prop`
Expected: PASS — no panics found.

- [ ] **Step 4: Commit**

```bash
git add crates/xboot-core/src/iscsi/pdu.rs
git commit -m "test(iscsi): proptest decode robustness and encode invariants"
```

---

## Task 6: Fuzz target for `decode`

**Files:**
- Modify: `fuzz/Cargo.toml`
- Create: `fuzz/fuzz_targets/iscsi_pdu.rs`

- [ ] **Step 1: Add the fuzz target binary to `fuzz/Cargo.toml`**

Append after the existing `[[bin]]` blocks:

```toml
[[bin]]
name = "iscsi_pdu"
path = "fuzz_targets/iscsi_pdu.rs"
test = false
doc = false
```

- [ ] **Step 2: Create the fuzz target**

Create `fuzz/fuzz_targets/iscsi_pdu.rs`:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // decode must never panic, regardless of input.
    let _ = xboot_core::iscsi::decode(data);
});
```

- [ ] **Step 3: Verify the fuzz crate builds**

Run: `cargo build --manifest-path fuzz/Cargo.toml --bin iscsi_pdu`
Expected: builds successfully. (If `cargo-fuzz`/nightly is required to *run* it, building under the workspace toolchain is enough to confirm the target compiles; running is optional in CI.)

If a nightly toolchain with `cargo-fuzz` is available, optionally run a short smoke fuzz:

Run: `cargo +nightly fuzz run iscsi_pdu -- -max_total_time=30`
Expected: no crashes in 30 seconds.

- [ ] **Step 4: Commit**

```bash
git add fuzz/Cargo.toml fuzz/fuzz_targets/iscsi_pdu.rs
git commit -m "test(iscsi): cargo-fuzz target for PDU decode"
```

---

## Task 7: Final verification gate

**Files:** none (verification only)

- [ ] **Step 1: Full test suite**

Run: `cargo test`
Expected: PASS — all existing tests plus the new `iscsi` tests (text: 5, pdu unit + prop). No failures.

- [ ] **Step 2: Clippy across all targets**

Run: `cargo clippy --all-targets`
Expected: no warnings.

- [ ] **Step 3: Format check**

Run: `cargo fmt --check`
Expected: clean (no diff).

- [ ] **Step 4: Confirm fuzz target still builds**

Run: `cargo build --manifest-path fuzz/Cargo.toml --bin iscsi_pdu`
Expected: builds.

- [ ] **Step 5: Final commit (if formatting changed anything)**

```bash
git add -A
git commit -m "chore(iscsi): phase 05a verification gate green" --allow-empty
```

---

## Done When

- `xboot_core::iscsi::decode` parses all opcodes in §3.1 of the spec; unknown opcodes → `Request::Unsupported`; structurally corrupt input → typed `PduError`; never panics.
- All response builders in §3.2 encode correct BHS + 4-byte-padded data; `DataSegmentLength` always matches the payload.
- `text::parse_pairs`/`encode_pairs` round-trip (proptest green).
- Fuzz target `iscsi_pdu` compiles and finds no panics in a smoke run.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` all clean.

## Notes for the Implementer

- **No I/O, no state, no SCSI interpretation.** If you find yourself wanting to validate `CmdSN` ordering or decode a CDB's LBA, stop — that is 05c / 05b.
- **Byte access safety:** all BHS reader offsets are fixed constants ≤ 44 against a slice already checked to be ≥ 48 bytes. Do not index the *data* segment with unchecked offsets; iterate or slice it.
- **`continue_` / `final_`** have trailing underscores because `continue` and `final`-adjacent names read awkwardly; keep them consistent across request and response types.
