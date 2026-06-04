//! In-process fake iSCSI initiator for integration tests. Builds initiator PDUs as
//! bytes and feeds them through `decode` + `Connection::handle`, mirroring exactly
//! what the tokio transport does — but with no socket.

#![cfg(test)]

use crate::iscsi::session::{Connection, Outbound};
use crate::iscsi::{decode, BHS_LEN};

/// Build a Login Request PDU (single text segment).
pub(crate) fn login_pdu(transit: bool, csg: u8, nsg: u8, keys: &[(&str, &str)]) -> Vec<u8> {
    let mut h = [0u8; BHS_LEN];
    h[0] = crate::iscsi::opcode::LOGIN_REQ;
    if transit {
        h[1] |= 0x80;
    }
    h[1] |= (csg & 0x3) << 2;
    h[1] |= nsg & 0x3;
    h[13] = 1; // ISID byte
    put32(&mut h, 16, 1); // ITT
    let text = encode_keys(keys);
    frame(h, &text)
}

/// Build a READ(10) SCSI Command PDU.
pub(crate) fn read10_pdu(itt: u32, lba: u32, blocks: u16) -> Vec<u8> {
    let mut h = [0u8; BHS_LEN];
    h[0] = crate::iscsi::opcode::SCSI_COMMAND;
    h[1] = 0xC0; // F + R (read)
    put32(&mut h, 16, itt);
    put32(&mut h, 20, blocks as u32 * 512); // EDTL
    h[32] = 0x28; // CDB[0] READ(10)
    h[34..38].copy_from_slice(&lba.to_be_bytes()); // CDB LBA at CDB byte 2 (= BHS 32+2)
    h[39..41].copy_from_slice(&blocks.to_be_bytes()); // CDB length at CDB byte 7
    frame(h, &[])
}

/// Build a WRITE(10) SCSI Command PDU with all data immediate.
pub(crate) fn write10_immediate_pdu(itt: u32, lba: u32, blocks: u16, data: &[u8]) -> Vec<u8> {
    let mut h = [0u8; BHS_LEN];
    h[0] = crate::iscsi::opcode::SCSI_COMMAND;
    h[1] = 0xA0; // F + W (write)
    put32(&mut h, 16, itt);
    put32(&mut h, 20, data.len() as u32); // EDTL
    h[32] = 0x2a; // WRITE(10)
    h[34..38].copy_from_slice(&lba.to_be_bytes());
    h[39..41].copy_from_slice(&blocks.to_be_bytes());
    frame(h, data)
}

/// Build a Logout Request PDU.
pub(crate) fn logout_pdu(itt: u32) -> Vec<u8> {
    let mut h = [0u8; BHS_LEN];
    h[0] = crate::iscsi::opcode::LOGOUT_REQ;
    h[1] = 0x80; // close session
    put32(&mut h, 16, itt);
    frame(h, &[])
}

/// Feed one raw initiator PDU into the connection, returning the responses.
pub(crate) fn step(conn: &mut Connection, pdu: &[u8]) -> Vec<Outbound> {
    let (req, _used) = decode(pdu).expect("testkit builds valid PDUs");
    conn.handle(req, &pdu[..BHS_LEN])
}

// ---- local BHS helpers (mirror pdu.rs; kept private to testkit) ----

fn put32(h: &mut [u8; BHS_LEN], off: usize, v: u32) {
    h[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

fn encode_keys(keys: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in keys {
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
        out.push(0);
    }
    out
}

fn frame(h: [u8; BHS_LEN], data: &[u8]) -> Vec<u8> {
    let mut h = h;
    // DataSegmentLength in the 24-bit field at bytes 5..8.
    let len = data.len() as u32;
    h[5] = (len >> 16) as u8;
    h[6] = (len >> 8) as u8;
    h[7] = len as u8;
    let mut pdu = h.to_vec();
    pdu.extend_from_slice(data);
    while !pdu.len().is_multiple_of(4) {
        pdu.push(0);
    }
    pdu
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::registry::TargetRegistry;
    use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
    use crate::iscsi::session::{Connection, Outbound, Stage};
    use crate::storage::BackingStore;
    use crate::volume::{RamOverlay, Volume};
    use std::io;
    use std::sync::Arc;

    const IQN: &str = "iqn.2026-06.dev.xboot:client-01";

    struct MemStore(Vec<u8>);
    impl BackingStore for MemStore {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let s = offset as usize;
            buf.copy_from_slice(&self.0[s..s + buf.len()]);
            Ok(())
        }
    }

    fn conn() -> Connection {
        let vol = Volume::new(
            Box::new(MemStore(vec![0u8; 4096])),
            Box::new(RamOverlay::new()),
        );
        let mut reg = TargetRegistry::new();
        reg.insert(IQN, ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]));
        Connection::new(Arc::new(reg))
    }

    #[test]
    fn full_cycle_login_write_read_logout() {
        let mut c = conn();

        // Login -> FullFeature.
        let out = step(&mut c, &login_pdu(true, 1, 1, &[("TargetName", IQN)]));
        assert!(matches!(out[0], Outbound::Login(_)));
        assert_eq!(c.stage(), Stage::FullFeature);

        // WRITE block 3 with 0x77, all immediate.
        let payload = vec![0x77u8; 512];
        let out = step(&mut c, &write10_immediate_pdu(40, 3, 1, &payload));
        let Outbound::ScsiResp(r) = &out[0] else {
            panic!("expected SCSI Response");
        };
        assert_eq!(r.status, 0x00);

        // READ block 3 back -> Data-In carrying our bytes.
        let out = step(&mut c, &read10_pdu(41, 3, 1));
        let Outbound::DataIn(d) = &out[0] else {
            panic!("expected Data-In");
        };
        assert!(d.has_status);
        assert_eq!(d.data, payload);

        // Logout -> closing.
        let out = step(&mut c, &logout_pdu(42));
        assert!(matches!(out[0], Outbound::Logout(_)));
        assert_eq!(c.stage(), Stage::Closing);
    }
}
