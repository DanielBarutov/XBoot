//! iSCSI session layer (phase 05c): sans-I/O connection state machine.
//!
//! `Connection::handle` consumes one decoded `Request` (plus its raw 48-byte BHS,
//! needed only to echo into a Reject) and returns the response PDUs to transmit.
//! No sockets, no async — the tokio adapter in `transport.rs` does the I/O.
#![allow(dead_code)] // WRITE-path fields (in_flight/PendingWrite) wired in by Task 5

mod login;
mod params;

use crate::iscsi::registry::TargetRegistry;
use crate::iscsi::scsi::ScsiTarget;
use crate::iscsi::{
    LoginResponse, LogoutResponse, NopIn, R2t, Reject, Request, ScsiDataIn, ScsiResponse,
    TextResponse,
};
use params::SessionParams;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// How many commands beyond ExpCmdSN we let the initiator queue.
const QUEUE_DEPTH: u32 = 16;

/// iSCSI Reject reason codes (RFC 7143 §11.17.1) we emit.
pub mod reject {
    pub const PROTOCOL_ERROR: u8 = 0x04;
    pub const COMMAND_NOT_SUPPORTED: u8 = 0x05;
    pub const INVALID_PDU_FIELD: u8 = 0x0a;
}

/// Session lifecycle stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Login,
    FullFeature,
    Closing,
}

/// A target-to-initiator PDU the transport must encode and send.
#[derive(Debug, Clone)]
pub enum Outbound {
    Login(LoginResponse),
    ScsiResp(ScsiResponse),
    DataIn(ScsiDataIn),
    R2t(R2t),
    NopIn(NopIn),
    Text(TextResponse),
    Logout(LogoutResponse),
    TaskMgmt(crate::iscsi::TaskMgmtResponse),
    Reject(Reject),
}

impl Outbound {
    /// Serialize to wire bytes via the 05a builders.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Outbound::Login(p) => p.encode(),
            Outbound::ScsiResp(p) => p.encode(),
            Outbound::DataIn(p) => p.encode(),
            Outbound::R2t(p) => p.encode(),
            Outbound::NopIn(p) => p.encode(),
            Outbound::Text(p) => p.encode(),
            Outbound::Logout(p) => p.encode(),
            Outbound::TaskMgmt(p) => p.encode(),
            Outbound::Reject(p) => p.encode(),
        }
    }
}

/// Write data being gathered for one command until EDTL bytes have arrived.
pub(crate) struct PendingWrite {
    pub(crate) cmd: crate::iscsi::ScsiCommand,
    pub(crate) buf: Vec<u8>,
    pub(crate) received: u32,
    pub(crate) r2t_sn: u32,
}

/// One iSCSI connection (= one session, single-connection in v1).
pub struct Connection {
    registry: Arc<RwLock<TargetRegistry>>,
    stage: Stage,
    params: SessionParams,
    target: Option<Arc<ScsiTarget>>,
    stat_sn: u32,
    exp_cmd_sn: u32,
    in_flight: HashMap<u32, PendingWrite>,
}

impl Connection {
    pub fn new(registry: Arc<RwLock<TargetRegistry>>) -> Self {
        Self {
            registry,
            stage: Stage::Login,
            params: SessionParams::default(),
            target: None,
            stat_sn: 0,
            exp_cmd_sn: 0,
            in_flight: HashMap::new(),
        }
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The command window we advertise: ExpCmdSN .. ExpCmdSN + QUEUE_DEPTH.
    pub(crate) fn max_cmd_sn(&self) -> u32 {
        self.exp_cmd_sn.wrapping_add(QUEUE_DEPTH)
    }

    /// Return the current StatSN, then advance it (one per response-bearing PDU).
    pub(crate) fn next_stat_sn(&mut self) -> u32 {
        let s = self.stat_sn;
        self.stat_sn = self.stat_sn.wrapping_add(1);
        s
    }

    /// Handle one decoded request. `bhs` is the raw 48-byte header, echoed into a
    /// Reject when needed. Returns the PDUs to transmit (possibly empty).
    pub fn handle(&mut self, req: Request, bhs: &[u8]) -> Vec<Outbound> {
        match (self.stage, req) {
            (Stage::Login, Request::Login(lr)) => login::handle_login(self, lr),
            (Stage::FullFeature, req) => self.handle_full_feature(req, bhs),
            // Anything else (e.g. a SCSI command before login completes) is a
            // protocol error: reject and close.
            (_, _) => {
                self.stage = Stage::Closing;
                vec![self.reject(reject::PROTOCOL_ERROR, bhs)]
            }
        }
    }

    /// Build a Reject echoing the offending header, advancing StatSN.
    pub(crate) fn reject(&mut self, reason: u8, bhs: &[u8]) -> Outbound {
        let mut header = bhs.to_vec();
        header.resize(crate::iscsi::BHS_LEN, 0);
        Outbound::Reject(Reject {
            reason,
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
            header,
        })
    }

    fn handle_full_feature(&mut self, req: Request, bhs: &[u8]) -> Vec<Outbound> {
        match req {
            Request::ScsiCommand(cmd) => self.scsi_command(cmd, bhs),
            Request::DataOut(d) => self.data_out(d, bhs),
            Request::NopOut(n) => self.nop_in(n),
            Request::Text(t) => self.text_response(t),
            Request::TaskMgmt(t) => self.task_mgmt(t),
            Request::Logout(l) => self.logout(l),
            Request::Login(_) => vec![self.reject(reject::PROTOCOL_ERROR, bhs)],
            Request::Unsupported { .. } => vec![self.reject(reject::COMMAND_NOT_SUPPORTED, bhs)],
        }
    }

    fn nop_in(&mut self, n: crate::iscsi::NopOut) -> Vec<Outbound> {
        vec![Outbound::NopIn(NopIn {
            lun: n.lun,
            itt: n.itt,
            ttt: 0xffff_ffff,
            stat_sn: self.stat_sn, // Nop-In as a reply does not consume a StatSN
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
            data: n.data,
        })]
    }

    fn text_response(&mut self, t: crate::iscsi::TextRequest) -> Vec<Outbound> {
        // Minimal SendTargets: advertise the one target this connection serves.
        let mut keys = Vec::new();
        if t.text.iter().any(|(k, _)| k == "SendTargets") {
            for iqn in self.registry.read().unwrap().iqns() {
                keys.push(("TargetName".to_string(), iqn));
            }
        }
        vec![Outbound::Text(TextResponse {
            final_: true,
            continue_: false,
            itt: t.itt,
            ttt: 0xffff_ffff,
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
            text: keys,
        })]
    }

    fn task_mgmt(&mut self, t: crate::iscsi::TaskMgmt) -> Vec<Outbound> {
        vec![Outbound::TaskMgmt(crate::iscsi::TaskMgmtResponse {
            response: 0x00, // function complete
            itt: t.itt,
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
        })]
    }

    fn logout(&mut self, _l: crate::iscsi::LogoutRequest) -> Vec<Outbound> {
        self.stage = Stage::Closing;
        vec![Outbound::Logout(LogoutResponse {
            response: 0x00, // connection or session closed successfully
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
        })]
    }

    /// Execute a SCSI command. READ data rides back in Data-In PDUs (status on the
    /// final one); WRITE is handled in Task 5; non-data commands return a SCSI
    /// Response. `bhs` is kept for Task 5's protocol rejects.
    fn scsi_command(&mut self, cmd: crate::iscsi::ScsiCommand, _bhs: &[u8]) -> Vec<Outbound> {
        tracing::info!(
            "iscsi: SCSI cmd lun={} cdb={:02x?} edtl={} read={} write={}",
            cmd.lun, &cmd.cdb[..cmd.cdb.len().min(10)], cmd.edtl, cmd.read, cmd.write
        );
        let target = match &self.target {
            Some(t) => t.clone(),
            None => {
                tracing::warn!("iscsi: SCSI command but no target bound!");
                return vec![self.reject(reject::PROTOCOL_ERROR, _bhs)];
            }
        };
        self.exp_cmd_sn = self.exp_cmd_sn.wrapping_add(1);

        // WRITE: gather data before executing.
        if cmd.write {
            return self.begin_write(cmd, _bhs);
        }

        let outcome = target.execute(&cmd, &cmd.data);
        if outcome.status != 0x00 {
            tracing::warn!(
                "iscsi: SCSI CHECK CONDITION cdb[0]=0x{:02x} status=0x{:02x} sense={:02x?}",
                cmd.cdb[0], outcome.status, &outcome.sense[..outcome.sense.len().min(14)]
            );
        }
        // READ that produced data -> chunked Data-In with status on the final PDU.
        if cmd.read && outcome.status == 0x00 && !outcome.data.is_empty() {
            return self.data_in_chunks(&cmd, outcome.data);
        }
        // Everything else (TEST UNIT READY, INQUIRY, CHECK CONDITION, ...) -> SCSI Response.
        let residual = cmd.edtl.saturating_sub(outcome.data.len() as u32);
        vec![Outbound::ScsiResp(ScsiResponse {
            response: 0x00, // command completed at target
            status: outcome.status,
            itt: cmd.itt,
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
            residual,
            sense: outcome.sense,
        })]
    }

    /// Split read data into Data-In PDUs of at most MaxRecvDataSegmentLength bytes,
    /// status collapsed onto the final PDU.
    fn data_in_chunks(&mut self, cmd: &crate::iscsi::ScsiCommand, data: Vec<u8>) -> Vec<Outbound> {
        let seg = self.params.max_recv_data_segment_length.max(512) as usize;
        let mut out = Vec::new();
        let mut offset = 0usize;
        let mut data_sn = 0u32;
        let total = data.len();
        while offset < total {
            let end = (offset + seg).min(total);
            let is_last = end == total;
            out.push(Outbound::DataIn(ScsiDataIn {
                final_: is_last,
                ack: false,
                has_status: is_last,
                status: 0x00, // GOOD; meaningful only on the final (status) PDU
                lun: cmd.lun,
                itt: cmd.itt,
                ttt: 0xffff_ffff,
                stat_sn: if is_last {
                    self.next_stat_sn()
                } else {
                    self.stat_sn
                },
                exp_cmd_sn: self.exp_cmd_sn,
                max_cmd_sn: self.max_cmd_sn(),
                data_sn,
                buffer_offset: offset as u32,
                data: data[offset..end].to_vec(),
            }));
            offset = end;
            data_sn += 1;
        }
        out
    }

    /// Start a WRITE: stash any immediate/unsolicited data; if more is needed,
    /// solicit it with an R2T, else execute immediately.
    fn begin_write(&mut self, cmd: crate::iscsi::ScsiCommand, bhs: &[u8]) -> Vec<Outbound> {
        let edtl = cmd.edtl;
        let immediate = cmd.data.clone();
        if immediate.len() as u32 > edtl {
            return vec![self.reject(reject::PROTOCOL_ERROR, bhs)];
        }
        let mut buf = vec![0u8; edtl as usize];
        buf[..immediate.len()].copy_from_slice(&immediate);
        let received = immediate.len() as u32;
        let itt = cmd.itt;
        let pending = PendingWrite {
            cmd,
            buf,
            received,
            r2t_sn: 0,
        };
        if received == edtl {
            return self.finish_write(pending);
        }
        // Solicit the remainder with a single R2T (bounded by MaxBurstLength).
        let want = (edtl - received).min(self.params.max_burst_length);
        let lun = pending.cmd.lun;
        let r2t_sn = pending.r2t_sn;
        let offset = received;
        self.in_flight.insert(itt, pending);
        vec![Outbound::R2t(R2t {
            lun,
            itt,
            ttt: itt, // reuse ITT as the transfer tag; unique per in-flight command
            stat_sn: self.stat_sn, // R2T does not consume a StatSN
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
            r2t_sn,
            buffer_offset: offset,
            desired_length: want,
        })]
    }

    /// Append one Data-Out burst; when EDTL is satisfied, execute and respond.
    fn data_out(&mut self, d: crate::iscsi::ScsiDataOut, bhs: &[u8]) -> Vec<Outbound> {
        let Some(p) = self.in_flight.get_mut(&d.itt) else {
            return vec![self.reject(reject::PROTOCOL_ERROR, bhs)];
        };
        let start = d.buffer_offset as usize;
        let end = start + d.data.len();
        if end > p.buf.len() {
            self.in_flight.remove(&d.itt);
            return vec![self.reject(reject::PROTOCOL_ERROR, bhs)];
        }
        p.buf[start..end].copy_from_slice(&d.data);
        p.received += d.data.len() as u32;

        let edtl = p.cmd.edtl;
        if p.received < edtl {
            // Solicit the next burst.
            p.r2t_sn += 1;
            let r2t_sn = p.r2t_sn;
            let offset = p.received;
            let lun = p.cmd.lun;
            let want = (edtl - offset).min(self.params.max_burst_length);
            return vec![Outbound::R2t(R2t {
                lun,
                itt: d.itt,
                ttt: d.itt,
                stat_sn: self.stat_sn,
                exp_cmd_sn: self.exp_cmd_sn,
                max_cmd_sn: self.max_cmd_sn(),
                r2t_sn,
                buffer_offset: offset,
                desired_length: want,
            })];
        }
        let pending = self.in_flight.remove(&d.itt).expect("present above");
        self.finish_write(pending)
    }

    /// Run the assembled write through the SCSI target and build the SCSI Response.
    fn finish_write(&mut self, p: PendingWrite) -> Vec<Outbound> {
        let target = self.target.clone().expect("target resolved in FullFeature");
        let outcome = target.execute(&p.cmd, &p.buf);
        vec![Outbound::ScsiResp(ScsiResponse {
            response: 0x00,
            status: outcome.status,
            itt: p.cmd.itt,
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
            residual: 0,
            sense: outcome.sense,
        })]
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
    use crate::storage::BackingStore;
    use crate::volume::{RamOverlay, Volume};
    use std::io;

    pub(crate) const IQN: &str = "iqn.2026-06.dev.xboot:client-01";

    pub(crate) struct MemStore(pub Vec<u8>);
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

    /// A registry with one target (IQN above) over an in-memory master of `bytes`.
    pub(crate) fn registry(bytes: Vec<u8>) -> Arc<RwLock<TargetRegistry>> {
        let vol = Volume::new(Box::new(MemStore(bytes)), Box::new(RamOverlay::new()));
        let mut reg = TargetRegistry::new();
        reg.insert(IQN, ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]));
        Arc::new(RwLock::new(reg))
    }

    /// A fresh connection over a 4096-byte (8-block) target.
    pub(crate) fn conn() -> Connection {
        Connection::new(registry(vec![0u8; 4096]))
    }
}

#[cfg(test)]
mod login_tests {
    use super::test_support::*;
    use super::*;
    use crate::iscsi::LoginRequest;

    fn login_req(transit: bool, csg: u8, nsg: u8, keys: &[(&str, &str)]) -> LoginRequest {
        LoginRequest {
            transit,
            continue_: false,
            csg,
            nsg,
            version_max: 0,
            version_min: 0,
            isid: [0, 0, 0, 0, 0, 1],
            tsih: 0,
            itt: 1,
            cid: 0,
            cmd_sn: 0,
            exp_stat_sn: 0,
            text: keys
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn collapsed_login_to_full_feature_succeeds() {
        let mut c = conn();
        // CSG=0 (Security), NSG=3 (FullFeature) — skip auth, transit directly.
        let req = login_req(
            true,
            0,
            3,
            &[("TargetName", IQN), ("MaxBurstLength", "16384")],
        );
        let out = c.handle(Request::Login(req), &[0u8; 48]);
        assert_eq!(out.len(), 1);
        let Outbound::Login(resp) = &out[0] else {
            panic!("expected LoginResponse");
        };
        assert_eq!(resp.status_class, 0);
        assert!(resp.transit);
        assert_eq!(resp.nsg, 3);
        assert_eq!(c.stage(), Stage::FullFeature);
    }

    #[test]
    fn unknown_target_fails_login() {
        let mut c = conn();
        let req = login_req(true, 0, 3, &[("TargetName", "iqn.2026-06.dev.xboot:ghost")]);
        let out = c.handle(Request::Login(req), &[0u8; 48]);
        let Outbound::Login(resp) = &out[0] else {
            panic!("expected LoginResponse");
        };
        assert_eq!(resp.status_class, 0x02); // target/initiator error
        assert_eq!(c.stage(), Stage::Closing);
    }

    #[test]
    fn missing_target_name_fails_login() {
        let mut c = conn();
        let req = login_req(true, 0, 3, &[("HeaderDigest", "None")]);
        let out = c.handle(Request::Login(req), &[0u8; 48]);
        let Outbound::Login(resp) = &out[0] else {
            panic!("expected LoginResponse");
        };
        assert_eq!(resp.status_class, 0x02);
    }

    #[test]
    fn negotiated_keys_are_echoed_in_response() {
        let mut c = conn();
        let req = login_req(
            true,
            0,
            3,
            &[("TargetName", IQN), ("MaxBurstLength", "16384")],
        );
        let out = c.handle(Request::Login(req), &[0u8; 48]);
        let Outbound::Login(resp) = &out[0] else {
            panic!("expected LoginResponse");
        };
        let mbl = resp.text.iter().find(|(k, _)| k == "MaxBurstLength");
        assert_eq!(
            mbl,
            Some(&("MaxBurstLength".to_string(), "16384".to_string()))
        );
    }

    #[test]
    fn scsi_command_before_full_feature_is_protocol_reject() {
        let mut c = conn();
        // A NopOut in the Login stage is out of place -> reject + close.
        let nop = crate::iscsi::NopOut {
            lun: 0,
            itt: 5,
            ttt: 0xffff_ffff,
            cmd_sn: 0,
            exp_stat_sn: 0,
            data: vec![],
        };
        let out = c.handle(Request::NopOut(nop), &[0u8; 48]);
        assert!(matches!(out[0], Outbound::Reject(_)));
        assert_eq!(c.stage(), Stage::Closing);
    }
}

#[cfg(test)]
mod read_tests {
    use super::test_support::*;
    use super::*;
    use crate::iscsi::{Request, ScsiCommand};

    /// Drive a connection straight to FullFeature for command tests.
    fn full_feature() -> Connection {
        let mut c = conn();
        let req = crate::iscsi::LoginRequest {
            transit: true,
            continue_: false,
            csg: 0,
            nsg: 3,
            version_max: 0,
            version_min: 0,
            isid: [0, 0, 0, 0, 0, 1],
            tsih: 0,
            itt: 1,
            cid: 0,
            cmd_sn: 0,
            exp_stat_sn: 0,
            text: vec![
                ("TargetName".into(), IQN.into()),
                ("MaxRecvDataSegmentLength".into(), "4096".into()),
            ],
        };
        c.handle(Request::Login(req), &[0u8; 48]);
        assert_eq!(c.stage(), Stage::FullFeature);
        c
    }

    fn read10(lba: u32, blocks: u16, edtl: u32, itt: u32) -> ScsiCommand {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x28; // READ(10)
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        ScsiCommand {
            final_: true,
            read: true,
            write: false,
            attr: 0,
            lun: 0,
            itt,
            edtl,
            cmd_sn: 0,
            exp_stat_sn: 0,
            cdb,
            data: Vec::new(),
        }
    }

    #[test]
    fn read_one_block_yields_one_data_in_with_status() {
        let mut c = full_feature();
        // 1 block = 512 bytes <= MaxRecvDataSegmentLength 4096 -> single Data-In.
        let out = c.handle(Request::ScsiCommand(read10(0, 1, 512, 10)), &[0u8; 48]);
        assert_eq!(out.len(), 1);
        let Outbound::DataIn(d) = &out[0] else {
            panic!("expected Data-In");
        };
        assert!(d.final_);
        assert!(d.has_status);
        assert_eq!(d.status, 0x00); // GOOD
        assert_eq!(d.data.len(), 512);
        assert_eq!(d.buffer_offset, 0);
    }

    #[test]
    fn read_chunks_by_max_recv_data_segment_length() {
        let mut c = full_feature();
        // Force two chunks: read 8 blocks (4096) with cap 2048.
        c.params.max_recv_data_segment_length = 2048;
        let out = c.handle(Request::ScsiCommand(read10(0, 8, 4096, 11)), &[0u8; 48]);
        // 4096 / 2048 = 2 Data-In PDUs.
        assert_eq!(out.len(), 2);
        let Outbound::DataIn(d0) = &out[0] else {
            panic!()
        };
        let Outbound::DataIn(d1) = &out[1] else {
            panic!()
        };
        assert!(!d0.final_ && !d0.has_status);
        assert_eq!(d0.data_sn, 0);
        assert_eq!(d0.buffer_offset, 0);
        assert_eq!(d0.data.len(), 2048);
        assert!(d1.final_ && d1.has_status);
        assert_eq!(d1.data_sn, 1);
        assert_eq!(d1.buffer_offset, 2048);
        assert_eq!(d1.data.len(), 2048);
    }

    #[test]
    fn read_check_condition_yields_scsi_response_with_sense() {
        let mut c = full_feature();
        // LBA 100 on an 8-block disk -> out of range -> CHECK CONDITION.
        let out = c.handle(Request::ScsiCommand(read10(100, 1, 512, 12)), &[0u8; 48]);
        assert_eq!(out.len(), 1);
        let Outbound::ScsiResp(r) = &out[0] else {
            panic!("expected SCSI Response");
        };
        assert_eq!(r.status, 0x02); // CHECK CONDITION
        assert!(!r.sense.is_empty());
    }
}

#[cfg(test)]
mod write_tests {
    use super::test_support::*;
    use super::*;
    use crate::iscsi::{Request, ScsiCommand, ScsiDataOut};

    fn full_feature() -> Connection {
        let mut c = conn();
        let req = crate::iscsi::LoginRequest {
            transit: true,
            continue_: false,
            csg: 0,
            nsg: 3,
            version_max: 0,
            version_min: 0,
            isid: [0, 0, 0, 0, 0, 1],
            tsih: 0,
            itt: 1,
            cid: 0,
            cmd_sn: 0,
            exp_stat_sn: 0,
            text: vec![("TargetName".into(), IQN.into())],
        };
        c.handle(Request::Login(req), &[0u8; 48]);
        c
    }

    fn write10(lba: u32, blocks: u16, edtl: u32, itt: u32, immediate: Vec<u8>) -> ScsiCommand {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x2a; // WRITE(10)
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&blocks.to_be_bytes());
        ScsiCommand {
            final_: true,
            read: false,
            write: true,
            attr: 0,
            lun: 0,
            itt,
            edtl,
            cmd_sn: 0,
            exp_stat_sn: 0,
            cdb,
            data: immediate,
        }
    }

    #[test]
    fn write_fully_immediate_executes_at_once() {
        let mut c = full_feature();
        let payload = vec![0xABu8; 512];
        // All 512 bytes arrive as immediate data -> no R2T, straight SCSI Response.
        let out = c.handle(
            Request::ScsiCommand(write10(1, 1, 512, 20, payload.clone())),
            &[0u8; 48],
        );
        assert_eq!(out.len(), 1);
        let Outbound::ScsiResp(r) = &out[0] else {
            panic!("expected SCSI Response");
        };
        assert_eq!(r.status, 0x00); // GOOD

        // Read it back: the bytes must have landed in the overlay.
        let mut cdb = [0u8; 16];
        cdb[0] = 0x28;
        cdb[2..6].copy_from_slice(&1u32.to_be_bytes());
        cdb[7..9].copy_from_slice(&1u16.to_be_bytes());
        let rd = ScsiCommand {
            final_: true,
            read: true,
            write: false,
            attr: 0,
            lun: 0,
            itt: 21,
            edtl: 512,
            cmd_sn: 0,
            exp_stat_sn: 0,
            cdb,
            data: vec![],
        };
        let back = c.handle(Request::ScsiCommand(rd), &[0u8; 48]);
        let Outbound::DataIn(d) = &back[0] else {
            panic!()
        };
        assert!(d.data.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn write_with_no_immediate_data_issues_r2t() {
        let mut c = full_feature();
        // No immediate data and 512 bytes expected -> target solicits with one R2T.
        let out = c.handle(
            Request::ScsiCommand(write10(0, 1, 512, 22, vec![])),
            &[0u8; 48],
        );
        assert_eq!(out.len(), 1);
        let Outbound::R2t(r) = &out[0] else {
            panic!("expected R2T");
        };
        assert_eq!(r.itt, 22);
        assert_eq!(r.buffer_offset, 0);
        assert_eq!(r.desired_length, 512);
        assert_eq!(r.r2t_sn, 0);
    }

    #[test]
    fn data_out_completes_a_solicited_write() {
        let mut c = full_feature();
        c.handle(
            Request::ScsiCommand(write10(0, 1, 512, 23, vec![])),
            &[0u8; 48],
        );
        // Send the solicited Data-Out (final), which should trigger execute + Response.
        let dout = ScsiDataOut {
            final_: true,
            lun: 0,
            itt: 23,
            ttt: 0,
            exp_stat_sn: 0,
            data_sn: 0,
            buffer_offset: 0,
            data: vec![0x5Au8; 512],
        };
        let out = c.handle(Request::DataOut(dout), &[0u8; 48]);
        assert_eq!(out.len(), 1);
        let Outbound::ScsiResp(r) = &out[0] else {
            panic!("expected SCSI Response");
        };
        assert_eq!(r.status, 0x00);
        assert!(c.in_flight.is_empty());
    }

    #[test]
    fn data_out_past_edtl_is_protocol_reject() {
        let mut c = full_feature();
        c.handle(
            Request::ScsiCommand(write10(0, 1, 512, 24, vec![])),
            &[0u8; 48],
        );
        let dout = ScsiDataOut {
            final_: true,
            lun: 0,
            itt: 24,
            ttt: 0,
            exp_stat_sn: 0,
            data_sn: 0,
            buffer_offset: 256,
            data: vec![0u8; 512], // 256 + 512 = 768 > EDTL 512
        };
        let out = c.handle(Request::DataOut(dout), &[0u8; 48]);
        assert!(matches!(out[0], Outbound::Reject(_)));
    }
}

#[cfg(test)]
mod control_tests {
    use super::test_support::*;
    use super::*;
    use crate::iscsi::{LogoutRequest, NopOut, Request, TaskMgmt, TextRequest};

    fn full_feature() -> Connection {
        let mut c = conn();
        let req = crate::iscsi::LoginRequest {
            transit: true,
            continue_: false,
            csg: 0,
            nsg: 3,
            version_max: 0,
            version_min: 0,
            isid: [0, 0, 0, 0, 0, 1],
            tsih: 0,
            itt: 1,
            cid: 0,
            cmd_sn: 0,
            exp_stat_sn: 0,
            text: vec![("TargetName".into(), IQN.into())],
        };
        c.handle(Request::Login(req), &[0u8; 48]);
        c
    }

    #[test]
    fn nop_out_is_echoed_as_nop_in() {
        let mut c = full_feature();
        let nop = NopOut {
            lun: 0,
            itt: 30,
            ttt: 0xffff_ffff,
            cmd_sn: 1,
            exp_stat_sn: 0,
            data: vec![1, 2, 3],
        };
        let out = c.handle(Request::NopOut(nop), &[0u8; 48]);
        let Outbound::NopIn(n) = &out[0] else {
            panic!("expected Nop-In")
        };
        assert_eq!(n.itt, 30);
        assert_eq!(n.data, vec![1, 2, 3]);
    }

    #[test]
    fn sendtargets_text_lists_the_target() {
        let mut c = full_feature();
        let t = TextRequest {
            final_: true,
            continue_: false,
            lun: 0,
            itt: 31,
            ttt: 0xffff_ffff,
            cmd_sn: 1,
            exp_stat_sn: 0,
            text: vec![("SendTargets".into(), "All".into())],
        };
        let out = c.handle(Request::Text(t), &[0u8; 48]);
        let Outbound::Text(r) = &out[0] else {
            panic!("expected Text Response")
        };
        assert!(r.text.iter().any(|(k, v)| k == "TargetName" && v == IQN));
    }

    #[test]
    fn task_mgmt_returns_function_complete() {
        let mut c = full_feature();
        let tm = TaskMgmt {
            function: 1,
            lun: 0,
            itt: 32,
            ref_task_tag: 0,
            cmd_sn: 1,
            exp_stat_sn: 0,
        };
        let out = c.handle(Request::TaskMgmt(tm), &[0u8; 48]);
        assert!(matches!(out[0], Outbound::TaskMgmt(_)));
        let bytes = out[0].encode();
        assert_eq!(bytes[0], crate::iscsi::opcode::TASK_MGMT_RESP);
        assert_eq!(bytes[2], 0x00); // function complete
    }

    #[test]
    fn logout_responds_and_closes() {
        let mut c = full_feature();
        let lo = LogoutRequest {
            reason: 0,
            itt: 33,
            cid: 0,
            cmd_sn: 1,
            exp_stat_sn: 0,
        };
        let out = c.handle(Request::Logout(lo), &[0u8; 48]);
        let Outbound::Logout(r) = &out[0] else {
            panic!("expected Logout Response")
        };
        assert_eq!(r.response, 0x00);
        assert_eq!(c.stage(), Stage::Closing);
    }
}
