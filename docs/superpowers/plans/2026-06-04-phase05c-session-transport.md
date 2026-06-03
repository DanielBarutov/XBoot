# Phase 05c — iSCSI Session & Transport Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive a full iSCSI session over TCP — login negotiation, READ/WRITE data transfer, logout — by gluing the 05a PDU codec and 05b `ScsiTarget` together behind a sans-I/O state machine plus a thin tokio adapter.

**Architecture:** A pure `Connection` state machine (`handle(req, bhs) -> Vec<Outbound>`) walks Login → FullFeature → Closing, resolving a client's `Arc<ScsiTarget>` from `TargetName` via a `TargetRegistry`. READ chunks data into Data-In PDUs (status on the final one); WRITE accepts immediate + unsolicited first burst and issues R2T for the rest, gathering Data-Out before one `execute` call. A tokio accept-loop spawns one task per connection and is the only async code. A `testkit` fake initiator exercises both the core directly and over loopback.

**Tech Stack:** Rust, `xboot-core` crate, `tokio` (net + io-util, added here), `proptest` (dev-dep, already present). New module tree `crates/xboot-core/src/iscsi/{session/,registry.rs,transport.rs,testkit.rs}`.

**Spec:** `docs/superpowers/specs/2026-06-04-phase05c-session-transport-design.md`

**Staging:** Tasks 1–7 build the sans-I/O core + fake initiator and are fully testable without sockets. Tasks 8–9 add the tokio transport and concurrency tests. Task 10 is the verification/merge gate.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/xboot-core/src/iscsi/session/params.rs` | `SessionParams` + RFC defaults + per-key negotiation helpers |
| `crates/xboot-core/src/iscsi/session/login.rs` | login stage walk + key negotiation, produces `LoginResponse` |
| `crates/xboot-core/src/iscsi/session/mod.rs` | `Connection`, `Outbound`, `handle` dispatch, READ/WRITE flows, `PendingWrite`, reject reasons |
| `crates/xboot-core/src/iscsi/registry.rs` | `TargetRegistry`: `TargetName` → `Arc<ScsiTarget>` |
| `crates/xboot-core/src/iscsi/transport.rs` | tokio `serve` accept loop + per-connection framing (only async file) |
| `crates/xboot-core/src/iscsi/testkit.rs` | fake initiator: build initiator PDUs, drive `Connection` directly and over `TcpStream` |
| `crates/xboot-core/src/iscsi/pdu.rs` | (modify) add `TaskMgmtResponse` builder |
| `crates/xboot-core/src/iscsi/mod.rs` | (modify) `pub mod session/registry/transport`; re-exports; `#[cfg(test)] mod testkit` |
| `crates/xboot-core/Cargo.toml` | (modify) add `tokio` dependency |

**Privacy:** `session`, `registry`, `transport`, `testkit` are sibling modules under `iscsi`. `Connection`'s internals stay private; tests live in-module or use `testkit`.

---

## Task 1: SessionParams + key negotiation

**Files:**
- Create: `crates/xboot-core/src/iscsi/session/params.rs`
- Modify: `crates/xboot-core/src/iscsi/mod.rs`

- [ ] **Step 1: Declare the `session` module**

In `crates/xboot-core/src/iscsi/mod.rs`, add after the existing `pub mod scsi;` line:

```rust
pub mod session;
```

- [ ] **Step 2: Create `session/mod.rs` shell declaring submodules**

Create `crates/xboot-core/src/iscsi/session/mod.rs` (just `params` for now; `login`
and the `Connection` core are added in Task 3, which replaces this file):

```rust
//! iSCSI session layer (phase 05c): sans-I/O connection state machine.

mod params;
```

- [ ] **Step 3: Write the failing tests for `SessionParams`**

Create `crates/xboot-core/src/iscsi/session/params.rs`:

```rust
//! Operational parameters negotiated during login (RFC 7143 subset).
#![allow(dead_code)] // wired into Connection in Task 3

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_rfc() {
        let p = SessionParams::default();
        assert_eq!(p.max_recv_data_segment_length, 8192);
        assert_eq!(p.first_burst_length, 65536);
        assert_eq!(p.max_burst_length, 262144);
        assert!(p.immediate_data);
        assert!(p.initial_r2t); // RFC default Yes until negotiated down
    }

    #[test]
    fn numeric_keys_take_the_minimum() {
        let mut p = SessionParams::default();
        // Initiator offers a smaller MaxBurstLength -> we take theirs.
        p.negotiate("MaxBurstLength", "16384");
        assert_eq!(p.max_burst_length, 16384);
        // Initiator offers a larger value than ours -> we keep ours (min).
        p.negotiate("FirstBurstLength", "1048576");
        assert_eq!(p.first_burst_length, 65536);
    }

    #[test]
    fn immediate_data_is_anded_initial_r2t_is_ored() {
        let mut p = SessionParams::default();
        p.negotiate("ImmediateData", "No"); // ours Yes AND theirs No -> No
        assert!(!p.immediate_data);
        let mut p2 = SessionParams::default();
        p2.negotiate("InitialR2T", "No"); // ours Yes OR theirs No -> No
        assert!(!p2.initial_r2t);
    }

    #[test]
    fn max_recv_is_declarative_per_direction() {
        let mut p = SessionParams::default();
        // The initiator declares the most it will accept; we remember it verbatim.
        p.negotiate("MaxRecvDataSegmentLength", "4096");
        assert_eq!(p.max_recv_data_segment_length, 4096);
    }

    #[test]
    fn unknown_key_is_ignored() {
        let mut p = SessionParams::default();
        p.negotiate("X-com.example-thing", "whatever"); // must not panic
        assert_eq!(p.max_burst_length, 262144);
    }
}
```

- [ ] **Step 4: Run to confirm it fails to compile**

Run: `cargo test -p xboot-core session::params:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type SessionParams`.

- [ ] **Step 5: Implement `SessionParams`**

Insert above the `#[cfg(test)]` block in `params.rs`:

```rust
/// Operational parameters. We start at RFC defaults and fold in the initiator's
/// offered keys with the correct per-key rule (min for sizes, AND/OR for the
/// two boolean flow keys, declarative for MaxRecvDataSegmentLength).
#[derive(Debug, Clone, Copy)]
pub struct SessionParams {
    /// Most bytes the *initiator* will accept in one Data-In data segment.
    pub max_recv_data_segment_length: u32,
    /// Max unsolicited (immediate + first burst) write bytes per command.
    pub first_burst_length: u32,
    /// Max bytes per solicited (R2T) burst.
    pub max_burst_length: u32,
    /// May the initiator send immediate data with the command?
    pub immediate_data: bool,
    /// Must the target solicit *all* write data via R2T (no unsolicited burst)?
    pub initial_r2t: bool,
}

impl Default for SessionParams {
    fn default() -> Self {
        Self {
            max_recv_data_segment_length: 8192,
            first_burst_length: 65536,
            max_burst_length: 262144,
            immediate_data: true,
            initial_r2t: true,
        }
    }
}

impl SessionParams {
    /// Fold one negotiated `Key=Value` pair into the parameters. Unknown keys are
    /// ignored. Malformed numeric values are ignored (keep the current value).
    pub fn negotiate(&mut self, key: &str, value: &str) {
        match key {
            "MaxRecvDataSegmentLength" => {
                if let Ok(v) = value.parse() {
                    self.max_recv_data_segment_length = v; // declarative: take theirs
                }
            }
            "FirstBurstLength" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.first_burst_length = self.first_burst_length.min(v);
                }
            }
            "MaxBurstLength" => {
                if let Ok(v) = value.parse::<u32>() {
                    self.max_burst_length = self.max_burst_length.min(v);
                }
            }
            "ImmediateData" => self.immediate_data &= value.eq_ignore_ascii_case("Yes"),
            "InitialR2T" => self.initial_r2t = self.initial_r2t && value.eq_ignore_ascii_case("Yes"),
            _ => {}
        }
    }
}
```

- [ ] **Step 6: Run to confirm pass**

Run: `cargo test -p xboot-core session::params:: 2>&1 | tail -10`
Expected: PASS (5 tests).

- [ ] **Step 7: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -5
git add crates/xboot-core/src/iscsi/mod.rs crates/xboot-core/src/iscsi/session/
git commit -m "feat(iscsi): SessionParams with RFC defaults + key negotiation

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: TargetRegistry

**Files:**
- Create: `crates/xboot-core/src/iscsi/registry.rs`
- Modify: `crates/xboot-core/src/iscsi/mod.rs`

- [ ] **Step 1: Declare the module**

In `crates/xboot-core/src/iscsi/mod.rs`, add after `pub mod session;`:

```rust
pub mod registry;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/xboot-core/src/iscsi/registry.rs`:

```rust
//! Maps an iSCSI TargetName (IQN) to the client's SCSI target.

use crate::iscsi::scsi::ScsiTarget;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
    use crate::storage::BackingStore;
    use crate::volume::{RamOverlay, Volume};
    use std::io;

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

    fn one_lun_target() -> ScsiTarget {
        let vol = Volume::new(Box::new(MemStore(vec![0u8; 4096])), Box::new(RamOverlay::new()));
        ScsiTarget::new(vec![Some(LogicalUnit::new(vol))])
    }

    #[test]
    fn lookup_hit_returns_the_target() {
        let mut reg = TargetRegistry::new();
        reg.insert("iqn.2026-06.dev.xboot:client-01", one_lun_target());
        assert!(reg.get("iqn.2026-06.dev.xboot:client-01").is_some());
    }

    #[test]
    fn lookup_miss_is_none() {
        let reg = TargetRegistry::new();
        assert!(reg.get("iqn.2026-06.dev.xboot:nope").is_none());
    }
}
```

- [ ] **Step 3: Run to confirm failure**

Run: `cargo test -p xboot-core registry:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type TargetRegistry`.

- [ ] **Step 4: Implement the registry**

Insert above the `#[cfg(test)]` block in `registry.rs`:

```rust
/// Shared, read-mostly map of TargetName -> the client's `ScsiTarget`. Built once
/// at startup (test/config helpers in 05c; real per-MAC binding is phase 07) and
/// shared across all connection tasks behind `Arc`.
#[derive(Default)]
pub struct TargetRegistry {
    targets: HashMap<String, Arc<ScsiTarget>>,
}

impl TargetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a target under its IQN.
    pub fn insert(&mut self, iqn: impl Into<String>, target: ScsiTarget) {
        self.targets.insert(iqn.into(), Arc::new(target));
    }

    /// Resolve a TargetName to its shared target, if registered.
    pub fn get(&self, iqn: &str) -> Option<Arc<ScsiTarget>> {
        self.targets.get(iqn).cloned()
    }
}
```

- [ ] **Step 5: Run to confirm pass**

Run: `cargo test -p xboot-core registry:: 2>&1 | tail -10`
Expected: PASS (2 tests).

- [ ] **Step 6: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -5
git add crates/xboot-core/src/iscsi/mod.rs crates/xboot-core/src/iscsi/registry.rs
git commit -m "feat(iscsi): TargetRegistry (TargetName -> Arc<ScsiTarget>)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Connection skeleton + Outbound + login handling

This task adds the `Connection` state machine, the `Outbound` response enum, the
`handle` entry point, reject reasons, and full login handling (target resolution,
key negotiation, stage transition). It also adds `session/login.rs`.

**Files:**
- Modify: `crates/xboot-core/src/iscsi/session/mod.rs`
- Create: `crates/xboot-core/src/iscsi/session/login.rs`

- [ ] **Step 1: Replace `session/mod.rs` with the connection core + login tests**

Replace the contents of `crates/xboot-core/src/iscsi/session/mod.rs` with:

```rust
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
use std::sync::Arc;

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
    registry: Arc<TargetRegistry>,
    stage: Stage,
    params: SessionParams,
    target: Option<Arc<ScsiTarget>>,
    stat_sn: u32,
    exp_cmd_sn: u32,
    in_flight: HashMap<u32, PendingWrite>,
}

impl Connection {
    pub fn new(registry: Arc<TargetRegistry>) -> Self {
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

    // FullFeature dispatch is filled in by Tasks 4-6.
    fn handle_full_feature(&mut self, req: Request, bhs: &[u8]) -> Vec<Outbound> {
        let _ = req;
        vec![self.reject(reject::COMMAND_NOT_SUPPORTED, bhs)]
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
    pub(crate) fn registry(bytes: Vec<u8>) -> Arc<TargetRegistry> {
        let vol = Volume::new(Box::new(MemStore(bytes)), Box::new(RamOverlay::new()));
        let mut reg = TargetRegistry::new();
        reg.insert(IQN, ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]));
        Arc::new(reg)
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
            text: keys.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn collapsed_login_to_full_feature_succeeds() {
        let mut c = conn();
        // Operational stage (1) transiting to FullFeature (1), declaring TargetName.
        let req = login_req(true, 1, 1, &[("TargetName", IQN), ("MaxBurstLength", "16384")]);
        let out = c.handle(Request::Login(req), &[0u8; 48]);
        assert_eq!(out.len(), 1);
        let Outbound::Login(resp) = &out[0] else {
            panic!("expected LoginResponse");
        };
        assert_eq!(resp.status_class, 0);
        assert!(resp.transit);
        assert_eq!(resp.nsg, 1);
        assert_eq!(c.stage(), Stage::FullFeature);
    }

    #[test]
    fn unknown_target_fails_login() {
        let mut c = conn();
        let req = login_req(true, 1, 1, &[("TargetName", "iqn.2026-06.dev.xboot:ghost")]);
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
        let req = login_req(true, 1, 1, &[("HeaderDigest", "None")]);
        let out = c.handle(Request::Login(req), &[0u8; 48]);
        let Outbound::Login(resp) = &out[0] else {
            panic!("expected LoginResponse");
        };
        assert_eq!(resp.status_class, 0x02);
    }

    #[test]
    fn negotiated_keys_are_echoed_in_response() {
        let mut c = conn();
        let req = login_req(true, 1, 1, &[("TargetName", IQN), ("MaxBurstLength", "16384")]);
        let out = c.handle(Request::Login(req), &[0u8; 48]);
        let Outbound::Login(resp) = &out[0] else {
            panic!("expected LoginResponse");
        };
        let mbl = resp.text.iter().find(|(k, _)| k == "MaxBurstLength");
        assert_eq!(mbl, Some(&("MaxBurstLength".to_string(), "16384".to_string())));
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
```

- [ ] **Step 2: Create `session/login.rs`**

Create `crates/xboot-core/src/iscsi/session/login.rs`:

```rust
//! Login negotiation: resolve the target, fold operational keys, advance stage.

use super::{Connection, Outbound, Stage};
use crate::iscsi::{LoginRequest, LoginResponse};

/// Handle a LoginRequest in the Login stage. Resolves TargetName, negotiates the
/// operational keys we care about, and (when the initiator sets transit→FullFeature)
/// moves the connection to FullFeature. On a missing/unknown target, fails the login
/// with status-class 0x02 and marks the connection Closing.
pub(super) fn handle_login(conn: &mut Connection, req: LoginRequest) -> Vec<Outbound> {
    // Initialize sequencing from the first login PDU.
    if conn.stat_sn == 0 {
        conn.stat_sn = req.exp_stat_sn;
    }
    conn.exp_cmd_sn = req.cmd_sn;

    // Resolve the target on first declaration of TargetName.
    if conn.target.is_none() {
        if let Some((_, iqn)) = req.text.iter().find(|(k, _)| k == "TargetName") {
            conn.target = conn.registry.get(iqn);
            if conn.target.is_none() {
                return vec![fail(conn, &req)];
            }
        } else if req.transit {
            // Transiting to FullFeature without ever naming a target: error.
            return vec![fail(conn, &req)];
        }
    }

    // Negotiate the operational keys; echo our agreed values back.
    let mut reply_keys: Vec<(String, String)> = Vec::new();
    for (k, v) in &req.text {
        conn.params.negotiate(k, v);
    }
    // Echo the keys whose negotiated value the initiator needs to know.
    reply_keys.push((
        "MaxBurstLength".into(),
        conn.params.max_burst_length.to_string(),
    ));
    reply_keys.push((
        "FirstBurstLength".into(),
        conn.params.first_burst_length.to_string(),
    ));
    reply_keys.push((
        "ImmediateData".into(),
        yes_no(conn.params.immediate_data),
    ));
    reply_keys.push(("InitialR2T".into(), yes_no(conn.params.initial_r2t)));
    reply_keys.push(("HeaderDigest".into(), "None".into()));
    reply_keys.push(("DataDigest".into(), "None".into()));

    let transit = req.transit && req.nsg == 1;
    if transit {
        conn.stage = Stage::FullFeature;
    }

    vec![Outbound::Login(LoginResponse {
        transit,
        continue_: false,
        csg: req.csg,
        nsg: req.nsg,
        version_max: 0,
        version_active: 0,
        isid: req.isid,
        tsih: if req.tsih == 0 { 1 } else { req.tsih },
        itt: req.itt,
        stat_sn: conn.next_stat_sn(),
        exp_cmd_sn: conn.exp_cmd_sn,
        max_cmd_sn: conn.max_cmd_sn(),
        status_class: 0,
        status_detail: 0,
        text: reply_keys,
    })]
}

/// A login failure response (status-class 0x02 = target/initiator error), closing.
fn fail(conn: &mut Connection, req: &LoginRequest) -> Outbound {
    conn.stage = Stage::Closing;
    Outbound::Login(LoginResponse {
        transit: false,
        continue_: false,
        csg: req.csg,
        nsg: req.csg,
        version_max: 0,
        version_active: 0,
        isid: req.isid,
        tsih: req.tsih,
        itt: req.itt,
        stat_sn: conn.next_stat_sn(),
        exp_cmd_sn: conn.exp_cmd_sn,
        max_cmd_sn: conn.max_cmd_sn(),
        status_class: 0x02,
        status_detail: 0x03, // "not found"
        text: Vec::new(),
    })
}

fn yes_no(b: bool) -> String {
    if b { "Yes".into() } else { "No".into() }
}
```

Note: `login.rs` reads `Connection`'s private fields (`stat_sn`, `exp_cmd_sn`, `target`, `registry`, `params`, `stage`) directly because it is a child module of `session`. Those fields are declared in `session/mod.rs`.

- [ ] **Step 3: Run to confirm pass**

Run: `cargo test -p xboot-core session:: 2>&1 | tail -25`
Expected: PASS — the 5 `login_tests` plus the 5 `params` tests.

- [ ] **Step 4: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -8
git add crates/xboot-core/src/iscsi/session/
git commit -m "feat(iscsi): Connection state machine + login negotiation

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: FullFeature READ path

Adds the FullFeature dispatch for SCSI commands and the READ data-in chunking.

**Files:**
- Modify: `crates/xboot-core/src/iscsi/session/mod.rs`

- [ ] **Step 1: Write the failing tests**

Add to `session/mod.rs`, after the `login_tests` module, a new test module:

```rust
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
            csg: 1,
            nsg: 1,
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
        // 8 blocks = 4096 bytes, segment cap 4096 -> two chunks? 4096 fits in one.
        // Use 9 blocks padded: instead request 8 blocks but cap is 4096 -> one chunk.
        // Force two chunks: read 8 blocks (4096) with cap 2048 by re-negotiating.
        c.params.max_recv_data_segment_length = 2048;
        let out = c.handle(Request::ScsiCommand(read10(0, 8, 4096, 11)), &[0u8; 48]);
        // 4096 / 2048 = 2 Data-In PDUs.
        assert_eq!(out.len(), 2);
        let Outbound::DataIn(d0) = &out[0] else { panic!() };
        let Outbound::DataIn(d1) = &out[1] else { panic!() };
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
```

Note: delete the line `use crate::iscsi::scsi::cdb_for_test as _;` — it is a leftover guard and there is no such item. The real imports are `super::*`, `test_support::*`, and the `iscsi::{Request, ScsiCommand}` line.

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p xboot-core session::read_tests 2>&1 | tail -20`
Expected: FAIL — the tests compile but every command hits the `COMMAND_NOT_SUPPORTED` reject (the placeholder `handle_full_feature`).

- [ ] **Step 3: Implement the SCSI-command dispatch + READ chunking**

In `session/mod.rs`, replace the placeholder `handle_full_feature` with:

```rust
    fn handle_full_feature(&mut self, req: Request, bhs: &[u8]) -> Vec<Outbound> {
        match req {
            Request::ScsiCommand(cmd) => self.scsi_command(cmd, bhs),
            _ => vec![self.reject(reject::COMMAND_NOT_SUPPORTED, bhs)],
        }
    }

    /// Execute a SCSI command. READ data rides back in Data-In PDUs (status on the
    /// final one); WRITE is handled in Task 5; non-data commands return a SCSI
    /// Response. `bhs` is kept for Task 5's protocol rejects.
    fn scsi_command(&mut self, cmd: crate::iscsi::ScsiCommand, _bhs: &[u8]) -> Vec<Outbound> {
        let target = match &self.target {
            Some(t) => t.clone(),
            None => return vec![self.reject(reject::PROTOCOL_ERROR, _bhs)],
        };
        self.exp_cmd_sn = self.exp_cmd_sn.wrapping_add(1);
        let outcome = target.execute(&cmd, &cmd.data);

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
                stat_sn: if is_last { self.next_stat_sn() } else { self.stat_sn },
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
```

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p xboot-core session::read_tests 2>&1 | tail -20`
Expected: PASS (3 read tests). Also run `cargo test -p xboot-core session:: 2>&1 | tail -5` — all session tests pass.

- [ ] **Step 5: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -8
git add crates/xboot-core/src/iscsi/session/mod.rs
git commit -m "feat(iscsi): FullFeature SCSI dispatch + READ Data-In chunking

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: FullFeature WRITE path (immediate + unsolicited + R2T)

**Files:**
- Modify: `crates/xboot-core/src/iscsi/session/mod.rs`

- [ ] **Step 1: Write the failing tests**

Add a new test module to `session/mod.rs`:

```rust
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
            csg: 1,
            nsg: 1,
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
        let out = c.handle(Request::ScsiCommand(write10(1, 1, 512, 20, payload.clone())), &[0u8; 48]);
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
            final_: true, read: true, write: false, attr: 0, lun: 0, itt: 21,
            edtl: 512, cmd_sn: 0, exp_stat_sn: 0, cdb, data: vec![],
        };
        let back = c.handle(Request::ScsiCommand(rd), &[0u8; 48]);
        let Outbound::DataIn(d) = &back[0] else { panic!() };
        assert!(d.data.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn write_with_no_immediate_data_issues_r2t() {
        let mut c = full_feature();
        // No immediate data and 512 bytes expected -> target solicits with one R2T.
        let out = c.handle(Request::ScsiCommand(write10(0, 1, 512, 22, vec![])), &[0u8; 48]);
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
        c.handle(Request::ScsiCommand(write10(0, 1, 512, 23, vec![])), &[0u8; 48]);
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
        c.handle(Request::ScsiCommand(write10(0, 1, 512, 24, vec![])), &[0u8; 48]);
        let dout = ScsiDataOut {
            final_: true, lun: 0, itt: 24, ttt: 0, exp_stat_sn: 0, data_sn: 0,
            buffer_offset: 256,
            data: vec![0u8; 512], // 256 + 512 = 768 > EDTL 512
        };
        let out = c.handle(Request::DataOut(dout), &[0u8; 48]);
        assert!(matches!(out[0], Outbound::Reject(_)));
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p xboot-core session::write_tests 2>&1 | tail -20`
Expected: FAIL — writes hit the `COMMAND_NOT_SUPPORTED` reject (no DataOut arm; WRITE not yet special-cased).

- [ ] **Step 3: Implement the WRITE path**

In `session/mod.rs`, add a `DataOut` arm to `handle_full_feature`:

```rust
    fn handle_full_feature(&mut self, req: Request, bhs: &[u8]) -> Vec<Outbound> {
        match req {
            Request::ScsiCommand(cmd) => self.scsi_command(cmd, bhs),
            Request::DataOut(d) => self.data_out(d, bhs),
            _ => vec![self.reject(reject::COMMAND_NOT_SUPPORTED, bhs)],
        }
    }
```

In `scsi_command`, special-case writes *before* calling `execute`. Replace the body of
`scsi_command` (from the `self.exp_cmd_sn` line onward) with:

```rust
        self.exp_cmd_sn = self.exp_cmd_sn.wrapping_add(1);

        // WRITE: gather data before executing.
        if cmd.write {
            return self.begin_write(cmd, _bhs);
        }

        let outcome = target.execute(&cmd, &cmd.data);
        if cmd.read && outcome.status == 0x00 && !outcome.data.is_empty() {
            return self.data_in_chunks(&cmd, outcome.data);
        }
        let residual = cmd.edtl.saturating_sub(outcome.data.len() as u32);
        vec![Outbound::ScsiResp(ScsiResponse {
            response: 0x00,
            status: outcome.status,
            itt: cmd.itt,
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
            residual,
            sense: outcome.sense,
        })]
```

Add these methods inside `impl Connection` (after `data_in_chunks`):

```rust
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
        let pending = PendingWrite { cmd, buf, received, r2t_sn: 0 };
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
```

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p xboot-core session::write_tests 2>&1 | tail -20`
Expected: PASS (4 write tests). Then `cargo test -p xboot-core session:: 2>&1 | tail -5` — all pass.

- [ ] **Step 5: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -8
git add crates/xboot-core/src/iscsi/session/mod.rs
git commit -m "feat(iscsi): FullFeature WRITE path (immediate + unsolicited + R2T)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Nop-Out, Text, Task-Mgmt, Logout, and wrong-stage rejects

Adds the remaining FullFeature opcodes and a minimal `TaskMgmtResponse` builder.

**Files:**
- Modify: `crates/xboot-core/src/iscsi/pdu.rs` (add `TaskMgmtResponse`)
- Modify: `crates/xboot-core/src/iscsi/mod.rs` (re-export it)
- Modify: `crates/xboot-core/src/iscsi/session/mod.rs`

- [ ] **Step 1: Add the `TaskMgmtResponse` builder to `pdu.rs`**

In `crates/xboot-core/src/iscsi/pdu.rs`, after the `LogoutResponse` impl block, add:

```rust
#[derive(Debug, Clone)]
pub struct TaskMgmtResponse {
    pub response: u8,
    pub itt: u32,
    pub stat_sn: u32,
    pub exp_cmd_sn: u32,
    pub max_cmd_sn: u32,
}

impl TaskMgmtResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut h = [0u8; BHS_LEN];
        h[0] = opcode::TASK_MGMT_RESP;
        h[1] = 0x80; // F always set
        h[2] = self.response; // 0x00 = Function Complete
        put32(&mut h, 16, self.itt);
        put32(&mut h, 24, self.stat_sn);
        put32(&mut h, 28, self.exp_cmd_sn);
        put32(&mut h, 32, self.max_cmd_sn);
        frame(h, &[])
    }
}
```

- [ ] **Step 2: Re-export it from `mod.rs`**

In `crates/xboot-core/src/iscsi/mod.rs`, add `TaskMgmtResponse` to the `pub use pdu::{...}` list (alphabetically near `TaskMgmt`).

- [ ] **Step 3: Write the failing tests**

Add a new test module to `session/mod.rs`:

```rust
#[cfg(test)]
mod control_tests {
    use super::test_support::*;
    use super::*;
    use crate::iscsi::{LogoutRequest, NopOut, Request, TaskMgmt, TextRequest};

    fn full_feature() -> Connection {
        let mut c = conn();
        let req = crate::iscsi::LoginRequest {
            transit: true, continue_: false, csg: 1, nsg: 1, version_max: 0, version_min: 0,
            isid: [0, 0, 0, 0, 0, 1], tsih: 0, itt: 1, cid: 0, cmd_sn: 0, exp_stat_sn: 0,
            text: vec![("TargetName".into(), IQN.into())],
        };
        c.handle(Request::Login(req), &[0u8; 48]);
        c
    }

    #[test]
    fn nop_out_is_echoed_as_nop_in() {
        let mut c = full_feature();
        let nop = NopOut { lun: 0, itt: 30, ttt: 0xffff_ffff, cmd_sn: 1, exp_stat_sn: 0, data: vec![1, 2, 3] };
        let out = c.handle(Request::NopOut(nop), &[0u8; 48]);
        let Outbound::NopIn(n) = &out[0] else { panic!("expected Nop-In") };
        assert_eq!(n.itt, 30);
        assert_eq!(n.data, vec![1, 2, 3]);
    }

    #[test]
    fn sendtargets_text_lists_the_target() {
        let mut c = full_feature();
        let t = TextRequest {
            final_: true, continue_: false, lun: 0, itt: 31, ttt: 0xffff_ffff,
            cmd_sn: 1, exp_stat_sn: 0,
            text: vec![("SendTargets".into(), "All".into())],
        };
        let out = c.handle(Request::Text(t), &[0u8; 48]);
        let Outbound::Text(r) = &out[0] else { panic!("expected Text Response") };
        assert!(r.text.iter().any(|(k, v)| k == "TargetName" && v == IQN));
    }

    #[test]
    fn task_mgmt_returns_function_complete() {
        let mut c = full_feature();
        let tm = TaskMgmt { function: 1, lun: 0, itt: 32, ref_task_tag: 0, cmd_sn: 1, exp_stat_sn: 0 };
        let out = c.handle(Request::TaskMgmt(tm), &[0u8; 48]);
        assert!(matches!(out[0], Outbound::TaskMgmt(_)));
        let bytes = out[0].encode();
        assert_eq!(bytes[0], crate::iscsi::opcode::TASK_MGMT_RESP);
        assert_eq!(bytes[2], 0x00); // function complete
    }

    #[test]
    fn logout_responds_and_closes() {
        let mut c = full_feature();
        let lo = LogoutRequest { reason: 0, itt: 33, cid: 0, cmd_sn: 1, exp_stat_sn: 0 };
        let out = c.handle(Request::Logout(lo), &[0u8; 48]);
        let Outbound::Logout(r) = &out[0] else { panic!("expected Logout Response") };
        assert_eq!(r.response, 0x00);
        assert_eq!(c.stage(), Stage::Closing);
    }
}
```

Because `Outbound` has no `TaskMgmt` variant yet, this will not compile until Step 4.

- [ ] **Step 4: Add the `TaskMgmt` Outbound variant + handlers**

In `session/mod.rs`:

1. Add to the `Outbound` enum (after `Logout`):

```rust
    TaskMgmt(crate::iscsi::TaskMgmtResponse),
```

2. Add to `Outbound::encode`'s match:

```rust
            Outbound::TaskMgmt(p) => p.encode(),
```

3. Extend `handle_full_feature` to the full opcode set:

```rust
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
            for iqn in self.registry.iqns() {
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

    fn logout(&mut self, l: crate::iscsi::LogoutRequest) -> Vec<Outbound> {
        self.stage = Stage::Closing;
        vec![Outbound::Logout(LogoutResponse {
            response: 0x00, // connection or session closed successfully
            stat_sn: self.next_stat_sn(),
            exp_cmd_sn: self.exp_cmd_sn,
            max_cmd_sn: self.max_cmd_sn(),
        })]
    }
```

Note: `text_response` uses `self.registry.iqns()`. Add that helper to `TargetRegistry`
in `registry.rs`:

```rust
    /// All registered target IQNs (used by SendTargets text negotiation).
    pub fn iqns(&self) -> Vec<String> {
        self.targets.keys().cloned().collect()
    }
```

- [ ] **Step 5: Run to confirm pass**

Run: `cargo test -p xboot-core session::control_tests 2>&1 | tail -20`
Expected: PASS (4 control tests). Then `cargo test -p xboot-core iscsi:: 2>&1 | tail -5` — all iSCSI tests pass.

- [ ] **Step 6: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -8
git add crates/xboot-core/src/iscsi/pdu.rs crates/xboot-core/src/iscsi/mod.rs crates/xboot-core/src/iscsi/session/mod.rs crates/xboot-core/src/iscsi/registry.rs
git commit -m "feat(iscsi): Nop-In, SendTargets text, task-mgmt, logout handling

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Fake initiator (testkit) + end-to-end core integration test

The fake initiator builds initiator-side PDUs as bytes and drives a `Connection`
directly (no socket). This proves the whole core path login→WRITE→READ→logout.

**Files:**
- Create: `crates/xboot-core/src/iscsi/testkit.rs`
- Modify: `crates/xboot-core/src/iscsi/mod.rs`

- [ ] **Step 1: Declare the test-only module**

In `crates/xboot-core/src/iscsi/mod.rs`, add:

```rust
#[cfg(test)]
mod testkit;
```

- [ ] **Step 2: Write the testkit with its own end-to-end test**

Create `crates/xboot-core/src/iscsi/testkit.rs`:

```rust
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
    while pdu.len() % 4 != 0 {
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
        let vol = Volume::new(Box::new(MemStore(vec![0u8; 4096])), Box::new(RamOverlay::new()));
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
```

This requires `Connection`, `Outbound`, and `Stage` to be reachable from `iscsi::session`. Add to `session/mod.rs` nothing extra — they are already `pub`. Ensure `iscsi/mod.rs` does **not** need to re-export them for the in-crate testkit (path `crate::iscsi::session::...` works because `pub mod session`).

- [ ] **Step 3: Run the end-to-end test**

Run: `cargo test -p xboot-core iscsi::testkit:: 2>&1 | tail -20`
Expected: PASS (`full_cycle_login_write_read_logout`).

If `read10_pdu`'s CDB offsets are off (the Data-In comes back zeroed), recheck: CDB
starts at BHS byte 32, so CDB byte 2 (LBA) is BHS byte 34, CDB byte 7 (length) is BHS
byte 39. The asserts above will catch a mismatch.

- [ ] **Step 4: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -8
git add crates/xboot-core/src/iscsi/testkit.rs crates/xboot-core/src/iscsi/mod.rs
git commit -m "test(iscsi): fake initiator testkit + login/write/read/logout cycle

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: tokio transport adapter

Adds the only async code: an accept loop + per-connection framing. `tokio` becomes a
dependency of `xboot-core`.

**Files:**
- Modify: `crates/xboot-core/Cargo.toml`
- Create: `crates/xboot-core/src/iscsi/transport.rs`
- Modify: `crates/xboot-core/src/iscsi/mod.rs`

- [ ] **Step 1: Add tokio to `xboot-core`**

In `crates/xboot-core/Cargo.toml`, under `[dependencies]`, add:

```toml
tokio = { workspace = true, features = ["net", "io-util", "rt"] }
```

(The workspace entry already enables `rt-multi-thread` and `macros`; crate-level
features are additive.)

- [ ] **Step 2: Declare the module**

In `crates/xboot-core/src/iscsi/mod.rs`, add after `pub mod registry;`:

```rust
pub mod transport;
```

- [ ] **Step 3: Write the failing loopback test**

Create `crates/xboot-core/src/iscsi/transport.rs`:

```rust
//! tokio TCP adapter — the only async code in the iSCSI stack. Accepts connections,
//! frames PDUs across reads, and drives a `Connection`.

use crate::iscsi::registry::TargetRegistry;
use crate::iscsi::session::{Connection, Stage};
use crate::iscsi::{decode, PduError, BHS_LEN};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
    use crate::iscsi::testkit;
    use crate::storage::BackingStore;
    use crate::volume::{RamOverlay, Volume};
    use std::io;

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

    fn registry() -> Arc<TargetRegistry> {
        let vol = Volume::new(Box::new(MemStore(vec![0u8; 4096])), Box::new(RamOverlay::new()));
        let mut reg = TargetRegistry::new();
        reg.insert(IQN, ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]));
        Arc::new(reg)
    }

    #[tokio::test]
    async fn loopback_login_write_read_logout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reg = registry();
        tokio::spawn(async move { serve(listener, reg).await });

        let mut sock = TcpStream::connect(addr).await.unwrap();

        // Login.
        sock.write_all(&testkit::login_pdu(true, 1, 1, &[("TargetName", IQN)])).await.unwrap();
        let resp = read_one_pdu(&mut sock).await;
        assert_eq!(resp[0] & 0x3f, crate::iscsi::opcode::LOGIN_RESP);

        // WRITE block 2 = 0x99 (immediate).
        let payload = vec![0x99u8; 512];
        sock.write_all(&testkit::write10_immediate_pdu(50, 2, 1, &payload)).await.unwrap();
        let resp = read_one_pdu(&mut sock).await;
        assert_eq!(resp[0] & 0x3f, crate::iscsi::opcode::SCSI_RESP);
        assert_eq!(resp[3], 0x00); // GOOD

        // READ block 2 back.
        sock.write_all(&testkit::read10_pdu(51, 2, 1)).await.unwrap();
        let resp = read_one_pdu(&mut sock).await;
        assert_eq!(resp[0] & 0x3f, crate::iscsi::opcode::DATA_IN);
        assert_eq!(&resp[BHS_LEN..BHS_LEN + 512], &payload[..]);
    }

    /// Read exactly one PDU (BHS + data + pad) off the socket.
    async fn read_one_pdu(sock: &mut TcpStream) -> Vec<u8> {
        let mut buf = vec![0u8; BHS_LEN];
        sock.read_exact(&mut buf).await.unwrap();
        let dlen = ((buf[5] as usize) << 16) | ((buf[6] as usize) << 8) | buf[7] as usize;
        let pad = (4 - dlen % 4) % 4;
        let mut rest = vec![0u8; dlen + pad];
        if !rest.is_empty() {
            sock.read_exact(&mut rest).await.unwrap();
        }
        buf.extend_from_slice(&rest);
        buf
    }
}
```

- [ ] **Step 4: Run to confirm failure**

Run: `cargo test -p xboot-core iscsi::transport:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function serve`.

- [ ] **Step 5: Implement `serve` + `handle_conn`**

Insert above the `#[cfg(test)]` block in `transport.rs`:

```rust
/// Accept connections forever, one task per connection.
pub async fn serve(listener: TcpListener, registry: Arc<TargetRegistry>) -> std::io::Result<()> {
    loop {
        let (sock, _peer) = listener.accept().await?;
        let reg = registry.clone();
        tokio::spawn(async move {
            let _ = handle_conn(sock, reg).await;
        });
    }
}

/// Drive one connection: read bytes, frame PDUs, hand each to the state machine,
/// write the responses, and stop when the session enters Closing.
async fn handle_conn(mut sock: TcpStream, registry: Arc<TargetRegistry>) -> std::io::Result<()> {
    let mut conn = Connection::new(registry);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];

    loop {
        // Drain every complete PDU currently in `buf`.
        loop {
            match decode(&buf) {
                Ok((req, used)) => {
                    let bhs = buf[..BHS_LEN].to_vec();
                    for out in conn.handle(req, &bhs) {
                        sock.write_all(&out.encode()).await?;
                    }
                    buf.drain(..used);
                    if conn.stage() == Stage::Closing {
                        sock.flush().await?;
                        return Ok(());
                    }
                }
                Err(PduError::ShortHeader) | Err(PduError::ShortData) => break,
                Err(_structural) => {
                    // Malformed framing: best effort close.
                    sock.flush().await?;
                    return Ok(());
                }
            }
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(()); // peer closed
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}
```

- [ ] **Step 6: Run to confirm pass**

Run: `cargo test -p xboot-core iscsi::transport:: 2>&1 | tail -20`
Expected: PASS (`loopback_login_write_read_logout`).

- [ ] **Step 7: Lint + commit**

```bash
cargo clippy -p xboot-core --all-targets 2>&1 | tail -8
git add crates/xboot-core/Cargo.toml crates/xboot-core/src/iscsi/transport.rs crates/xboot-core/src/iscsi/mod.rs
git commit -m "feat(iscsi): tokio TCP transport (accept loop + PDU framing)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 9: Concurrency / isolation integration test

Proves many clients run in parallel over loopback without cross-client interference.

**Files:**
- Modify: `crates/xboot-core/src/iscsi/transport.rs` (tests only)

- [ ] **Step 1: Write the concurrency test**

Add this test to the `tests` module in `transport.rs`. It registers several targets,
spawns the server once, and runs N clients concurrently, each writing a distinct byte
to the same LBA on its own target and reading it back — asserting no client sees another's
data.

```rust
    fn multi_registry(n: u8) -> Arc<TargetRegistry> {
        let mut reg = TargetRegistry::new();
        for i in 0..n {
            let vol = Volume::new(Box::new(MemStore(vec![0u8; 4096])), Box::new(RamOverlay::new()));
            reg.insert(format!("{IQN}-{i}"), ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]));
        }
        Arc::new(reg)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_clients_are_isolated() {
        let n: u8 = 8;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, multi_registry(n)));

        let mut handles = Vec::new();
        for i in 0..n {
            handles.push(tokio::spawn(async move {
                let iqn = format!("{IQN}-{i}");
                let mut sock = TcpStream::connect(addr).await.unwrap();
                sock.write_all(&testkit::login_pdu(true, 1, 1, &[("TargetName", &iqn)])).await.unwrap();
                let _ = read_one_pdu(&mut sock).await;

                let byte = 0x10 + i;
                let payload = vec![byte; 512];
                sock.write_all(&testkit::write10_immediate_pdu(1, 0, 1, &payload)).await.unwrap();
                let _ = read_one_pdu(&mut sock).await;

                sock.write_all(&testkit::read10_pdu(2, 0, 1)).await.unwrap();
                let resp = read_one_pdu(&mut sock).await;
                // Each client must read back exactly its own byte.
                assert!(resp[BHS_LEN..BHS_LEN + 512].iter().all(|&b| b == byte), "client {i} saw cross-talk");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }
```

- [ ] **Step 2: Run the concurrency test**

Run: `cargo test -p xboot-core iscsi::transport::tests::concurrent_clients_are_isolated 2>&1 | tail -20`
Expected: PASS — all 8 clients read back their own byte.

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/iscsi/transport.rs
git commit -m "test(iscsi): concurrent multi-client isolation over loopback

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 10: Final verification gate + merge

**Files:** none (verification only)

- [ ] **Step 1: Full workspace test run**

Run: `cargo test 2>&1 | tail -25`
Expected: all tests pass across the workspace (existing + new 05c tests).

- [ ] **Step 2: Format + lint gate**

Run: `cargo fmt --check && cargo clippy --all-targets 2>&1 | tail -10`
Expected: no formatting diffs, no clippy warnings. If `cargo fmt --check` reports diffs,
run `cargo fmt` and commit the result.

- [ ] **Step 3: Confirm public surface**

Verify these are reachable (used by phase 06/07 wiring later):
`xboot_core::iscsi::transport::serve`, `xboot_core::iscsi::registry::TargetRegistry`,
`xboot_core::iscsi::session::Connection`.

Run: `cargo doc -p xboot-core --no-deps 2>&1 | tail -5`
Expected: builds without errors.

- [ ] **Step 4: Confirm the PDU fuzz target still builds**

Run: `cargo +nightly fuzz build iscsi_pdu 2>&1 | tail -5` (skip if nightly/cargo-fuzz
not installed — the framing relies on the same `decode` the fuzzer already covers).

- [ ] **Step 5: Merge to main**

```bash
git checkout main
git merge --no-ff phase05c-session-transport -m "Merge phase 05c: iSCSI session state machine + tokio transport"
cargo test 2>&1 | tail -5
```

---

## Self-Review notes (spec coverage)

- Sans-I/O `Connection` + `handle` (spec §3/§4) — Tasks 3–6. ✓
- Login two-stage walk + pragmatic key negotiation (spec §4 login) — Tasks 1, 3. ✓
- READ chunking, status on final Data-In, CHECK CONDITION → SCSI Response (spec §4 read) — Task 4. ✓
- WRITE immediate + unsolicited + R2T gather, Data-Out past EDTL → reject (spec §4 write, §7) — Task 5. ✓
- Nop-In, SendTargets text, task-mgmt, logout, unsupported/wrong-stage reject (spec §4, §7) — Tasks 3, 6. ✓
- `TargetRegistry` TargetName routing (spec §5) — Task 2. ✓
- tokio accept loop + per-connection framing (spec §5) — Task 8. ✓
- Fake initiator, core + loopback integration, concurrency/isolation (spec §6) — Tasks 7–9. ✓
- Error handling table (spec §7) — rejects/close paths across Tasks 3–8. ✓
- Out-of-scope items (CHAP, digests, MC/S, CmdSN reorder, TOML wiring) — not implemented, per spec §8. ✓
- `tokio` added to `xboot-core` only here (spec §9) — Task 8. ✓
```
