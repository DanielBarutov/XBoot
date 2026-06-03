# Phase 05c — iSCSI Session & Transport Design

> Status: design approved 2026-06-04. Final sub-phase of Phase 05 (iSCSI Target).
> Depends on **05a** (PDU codec: `iscsi/pdu.rs`, `iscsi/text.rs`) and **05b**
> (SCSI command layer: `iscsi/scsi/`). See [roadmap](../plans/2026-06-02-xboot-roadmap.md) Phase 05.

## 1. Goal

Tie the two pure layers already built — 05a (`decode` / response-builder `.encode()`)
and 05b (`ScsiTarget::execute(&ScsiCommand, write_data) -> ScsiOutcome`) — to a real
TCP socket so an actual iSCSI initiator (iPXE, Windows) can log in and read/write a
client's virtual disk through a full session.

Phase-wide decisions (already fixed for all of Phase 05): **sans-I/O core + thin
async adapter** (not async-throughout); the `iscsi/` module lives in `xboot-core`
(not a separate crate); a standard RFC 7143 subset; `AuthMethod=None` (trusted LAN).

## 2. Scope decision

05c is delivered as **one spec with a staged implementation plan**: the sans-I/O
connection state machine and the fake-initiator integration are built and fully
working *before* the tokio TCP adapter is added. This keeps the hard part (protocol
logic) testable without sockets, then bolts on a thin transport.

## 3. Architecture & module layout

The protocol logic is **sans-I/O**: a pure state machine that consumes one decoded
`Request` plus its current state and returns zero-or-more response PDUs to transmit.
A **thin tokio adapter** performs the actual socket reads/writes and PDU framing. Only
the adapter touches tokio or sockets; everything else is synchronous and unit-testable.

New files under `crates/xboot-core/src/iscsi/`:

| File | Responsibility | I/O? |
|------|----------------|------|
| `session/mod.rs` | `Connection` — the sans-I/O per-connection state machine. `handle(req) -> Vec<Outbound>`. Holds login stage, sequence numbers, resolved target, in-flight WRITE buffers. | none |
| `session/login.rs` | Login negotiation: the two-stage walk (Security → Operational → FullFeature) and the key=value negotiation. | none |
| `session/params.rs` | `SessionParams` — negotiated/operational parameters with RFC defaults. | none |
| `registry.rs` | `TargetRegistry` — maps `TargetName` (IQN) → `Arc<ScsiTarget>`. Looked up during login. | none |
| `transport.rs` | tokio adapter: `TcpListener` accept loop, per-connection task, PDU framing across TCP reads, calls `Connection`, writes the bytes. **Only file that touches tokio/sockets.** | tokio |
| `testkit.rs` | Fake in-process initiator (test-support): drives login→READ/WRITE→logout against a `Connection` directly and over a loopback `TcpStream`. | test-only |

`iscsi/mod.rs` gains `pub mod session; pub mod registry; pub mod transport;` and
re-exports the public surface (`Connection`, `TargetRegistry`, `serve`).

### Sans-I/O core shape

```rust
// Pure: feed one decoded Request, get back PDUs to transmit. Never blocks, never does I/O.
impl Connection {
    fn handle(&mut self, req: Request) -> Vec<Outbound>;
}

/// A target-to-initiator PDU the transport must encode and send.
enum Outbound {
    Login(LoginResponse),
    ScsiResp(ScsiResponse),
    DataIn(ScsiDataIn),
    R2t(R2t),
    NopIn(NopIn),
    Text(TextResponse),
    Logout(LogoutResponse),
    Reject(Reject),
}
```

`Outbound::encode()` delegates to the matching 05a builder. The transport loop is then:
`read → decode (05a) → conn.handle(req) → for each out { write(out.encode()) }`.

## 4. The `Connection` state machine

Three stages (RFC 7143). PDUs that don't belong to the current stage are rejected.

```
                  LoginRequest (CSG, transit bit)
   [Login] ───────────────────────────────────────────────► [FullFeature]
      │   target replies LoginResponse, advancing CSG→NSG               │
      │   until NSG=FullFeature(1) + transit                            │
      └─ bad/missing TargetName → LoginResponse status-class 0x02       │
                                                                        │
   [FullFeature]: SCSI Command / Data-Out / Nop-Out / Text / Task-Mgmt  │
      │   Logout request                                                │
      ▼                                                                 ▼
   [Closing] ──── LogoutResponse, transport closes the TCP connection ──┘
```

### State held in `Connection`

- `stage`: `Login` | `FullFeature` | `Closing`
- `params: SessionParams` — negotiated values, populated during login
- `target: Option<Arc<ScsiTarget>>` — resolved from `TargetName` at login
- `stat_sn: u32` — our per-PDU status sequence number
- `exp_cmd_sn: u32` / `max_cmd_sn: u32` — the command window we advertise back
- `in_flight: HashMap<u32 /*itt*/, PendingWrite>` — partial WRITE state

### Login (`session/login.rs`)

Walk the standard two stages, then transit to FullFeature:

1. **SecurityNegotiation** (CSG=0): offer/accept `AuthMethod=None`. No CHAP.
2. **LoginOperationalNegotiation** (CSG=1): negotiate the keys that affect data flow.
3. **FullFeature** (NSG=1 + transit): session is live.

A login may arrive collapsed (initiator jumps straight to operational, or sends all
keys in one PDU); the state machine handles both the staged and collapsed forms. Login
text may also span multiple PDUs via the `continue_` (C) bit — accumulate until a PDU
without C, then respond.

**Key negotiation — pragmatic subset.** Actually negotiate the keys that matter,
respond `Irrelevant`/echo-default for the rest:

| Key | Rule |
|-----|------|
| `MaxRecvDataSegmentLength` | declarative per direction; remember the initiator's so we chunk Data-In within it; declare our own. |
| `FirstBurstLength` | negotiated = min(ours, theirs); bounds unsolicited write data. |
| `MaxBurstLength` | negotiated = min; bounds each solicited (R2T) burst. |
| `ImmediateData` | negotiated = (ours AND theirs); we accept Yes. |
| `InitialR2T` | negotiated = (ours OR theirs); we accept No (allow unsolicited first burst). |
| `HeaderDigest`, `DataDigest` | `None`. |
| `MaxConnections` | `1` (no MC/S). |
| `ErrorRecoveryLevel` | `0`. |
| `DefaultTime2Wait`, `DefaultTime2Retain`, `MaxOutstandingR2T`, data-ordering keys | sane fixed defaults. |
| `TargetName` | required from initiator; resolves the target. |
| `SessionType` | `Normal` (we don't serve discovery sessions in 05c; `SendTargets` answered minimally in FullFeature Text). |

`SessionParams` defaults follow the RFC (e.g. `FirstBurstLength=65536`,
`MaxBurstLength=262144`, `MaxRecvDataSegmentLength=8192`) and are overwritten by
negotiation.

### FullFeature per-command flows

**READ** — `execute()` returns `ScsiOutcome{data,status,sense}`. Split `data` into
`ScsiDataIn` PDUs of at most the initiator's `MaxRecvDataSegmentLength` each,
incrementing `data_sn` and `buffer_offset`. The **final** Data-In carries the status
(the `has_status`/S bit), so no separate SCSI Response is emitted on the GOOD path.
Compute residual against EDTL. On `CHECK CONDITION` (data empty), emit a `ScsiResponse`
carrying the sense instead.

**WRITE** — gather, then execute once:
1. WRITE command arrives. Immediate data (if any) + unsolicited first burst
   (≤ `FirstBurstLength`) is stashed in `PendingWrite` keyed by ITT, recording EDTL.
2. While `bytes_received < EDTL`: emit an `R2t` soliciting the next chunk
   (≤ `MaxBurstLength`), advancing `r2t_sn` and `buffer_offset`.
3. Incoming `Data-Out` PDUs append at their `buffer_offset`. When
   `bytes_received == EDTL`, call `ScsiTarget::execute(cmd, &assembled)` once, drop the
   entry, emit `ScsiResponse` (GOOD or the CHECK CONDITION sense from 05b).

**Nop-Out** → `NopIn` echo (keepalive; copy TTT/data). **Text** in FullFeature (e.g.
`SendTargets=All`) → minimal `TextResponse` listing the connected target. **Task-Mgmt**
→ a "function complete" response. **Unsupported / wrong-stage** → `Reject` with the
appropriate reason; never a panic.

### Sequence numbers (deliberately simple)

Single connection per session, so:
- `stat_sn` increments once per response-bearing PDU we send.
- We advertise `max_cmd_sn = exp_cmd_sn + QUEUE_DEPTH` (small constant) so the
  initiator may keep issuing commands.
- PDUs are processed in **arrival order**; no CmdSN-based reordering in v1 (valid for
  a single connection, ErrorRecoveryLevel=0).

## 5. Registry, transport & concurrency

### `TargetRegistry`

Built once at startup: `HashMap<String /*IQN*/, Arc<ScsiTarget>>`, wrapped in `Arc`
and shared across all connection tasks. Lookup happens during login when the initiator
declares `TargetName=iqn...`. Miss → login failure (status-class 0x02). For 05c the
registry is populated by test/config helpers; the real per-MAC client→IQN binding and
TOML wiring are **phase 07**.

### Transport (`transport.rs`) — the only async code

```
serve(listener, Arc<TargetRegistry>):
  loop { (sock, _) = listener.accept().await
         tokio::spawn(handle_conn(sock, registry.clone())) }

handle_conn(sock, registry):
  conn = Connection::new(registry)
  buf  = BytesMut::new()
  loop:
    n = sock.read_buf(&mut buf).await
    if n == 0 { break }                       // peer closed / reset
    loop {                                     // drain all complete PDUs
        match decode(&buf) {
            Ok((req, used)) => {
                for out in conn.handle(req) { sock.write_all(&out.encode()).await? }
                buf.advance(used)
                if conn.stage == Closing { flush; return }
            }
            Err(ShortHeader | ShortData) => break,   // need more bytes
            Err(structural)              => { send Reject; return }
        }
    }
```

**Framing** is the one fiddly part: a single TCP read may yield a partial PDU or
several. We keep a growing `BytesMut` and only `decode` once `BHS_LEN` + declared data
+ pad are present — `decode` already signals "need more" via `ShortHeader`/`ShortData`,
and caps oversized lengths via `MAX_DATA_SEGMENT` (05a).

### Concurrency & isolation

Each connection task is independent (`tokio::spawn`); the only shared state is the
per-client `Arc<ScsiTarget>`. Reads hit the shared master through the phase-04 cache
(already concurrency-safe under its stress test); writes land in *that client's own*
overlay. Different clients = different targets = different overlays, so no cross-client
interference by construction. A failing/panicking connection task cannot take down the
accept loop or other clients.

## 6. Fake initiator & testing

### `testkit` fake initiator

A struct speaking just enough initiator-side iSCSI to: send a login (security →
operational → full-feature), issue READ(10)/WRITE(10), feed/collect Data-In/Data-Out +
R2T, and logout. Driven two ways:

- **Core tests (no socket):** call `conn.handle()` directly and assert the exact
  response PDUs — fast, deterministic, the bulk of coverage.
- **Transport tests (loopback):** `serve()` on `127.0.0.1:0`, connect a real
  `TcpStream`, run the full byte-level round trip — proves framing and the async glue.

### Test layers

1. **Unit** — login stage transitions (staged and collapsed); key negotiation
   (min/AND/OR rules); READ chunking at a tiny `MaxRecvDataSegmentLength`; WRITE R2T
   gather flow; wrong-stage → `Reject`; sequence-number advance.
2. **Integration** — fake initiator: full `login → WRITE block → READ it back →
   logout`, asserting the written data survives via the COW overlay.
3. **Concurrency** — N initiators against N targets in parallel (loopback), asserting
   per-client isolation and no races (mirrors phase-04 stress test).
4. **Fuzz** — the existing `iscsi_pdu` fuzz target already covers the `decode` entry the
   framing relies on; no new target required.

## 7. Error handling

Never panic; every situation has a defined reaction:

| Situation | Reaction |
|-----------|----------|
| Partial PDU at end of a read | Not an error — keep buffering, read more. |
| `decode` → `ShortData` / `ShortHeader` | "Need more bytes" (buffer, read more). |
| `decode` → structural error (`UnexpectedAhs`, `DataSegmentTooLong`) | `Reject` with the matching reason, then close. |
| Unknown opcode (`Request::Unsupported`) | `Reject` ("command not supported"); connection stays up. |
| Valid PDU, wrong stage (e.g. SCSI command before FullFeature) | `Reject` ("protocol error"); close. |
| `TargetName` missing/unknown at login | `LoginResponse` status-class 0x02; close. |
| Data-Out `buffer_offset`/length past EDTL | `Reject` ("protocol error"); drop the `PendingWrite`. |
| SCSI-level problem (bad LBA, unsupported CDB) | Handled by 05b → `CHECK CONDITION` in the SCSI Response; not a connection error. |
| TCP read returns 0 / reset | Drop the connection task; the client's volatile overlay is discarded (matches "clean master each boot"). |

## 8. Out of scope (deferred)

- CHAP / any auth beyond `AuthMethod=None`.
- Header/Data digests (negotiated to `None`).
- MC/S (multiple connections per session); ERL>0 error recovery.
- CmdSN-based reordering / command queuing beyond arrival order.
- Real per-MAC client→IQN binding and config/TOML wiring (phase 07); registry is fed by
  helpers for now.
- TLS/IPsec.

## 9. Definition of done

The fake initiator completes a full `login → READ/WRITE → logout` cycle; concurrent
clients are isolated; the PDU fuzzer stays green; `cargo test`, `cargo fmt --check`, and
`cargo clippy --all-targets` are clean. `tokio` is added as a dependency of `xboot-core`
in this sub-phase only.
