# Phase 06b — TFTP server design

Part of the **Network boot** subsystem (master spec
`docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md`, §3.1, §4).
Phase 06 is split into three sub-phases:

- 06a — proxyDHCP (complete, merged)
- **06b — TFTP** (this document)
- 06c — HTTP (serves the per-MAC iPXE boot script)

## 1. Purpose & scope

Serve prebuilt iPXE binaries (`undionly.kpxe` for BIOS, `ipxe.efi` for UEFI x64) over
the TFTP protocol. This is step 2 of the PXE chainload: proxyDHCP (06a) told the client
"bootfile is X, server is Y"; now the client comes to fetch the file itself.

### Decisions

| Decision | Choice |
|----------|--------|
| Protocol level | **Full RFC 1350 (RRQ/DATA/ACK/ERROR) + RFC 2347/2348/2349 options (blksize, tsize, timeout)** |
| Socket model | **Well-known :69 for initial RRQ + ephemeral port per transfer (TID)** — RFC 1350 §4 compliant |
| Retransmission | **Exponential backoff on server side** (1s → 2s → 4s → 8s → 16s, max 5 retries) |
| Sorcerer's Apprentice | **Handled** — duplicate ACK re-sends last DATA block |
| Path safety | **Strict sandboxing** — all paths resolved relative to `tftp_root`, `..` and absolute paths rejected |
| File size | **Read entirely into memory** — iPXE binaries are ~150–300 KB, well within budget |
| Write support | **None** — WRQ answered with ERROR "Access violation" |
| I/O layer | **tokio `UdpSocket`** — same approach as proxyDHCP (06a), works identically on Linux and Windows |

### Out of scope (v1)

- WRQ (write requests) — server is read-only.
- TFTP blocksize negotiation beyond server's preferred cap (1468 bytes — fits Ethernet MTU comfortably).
- Multicast TFTP (RFC 2090).
- Windows `npcap` / raw L2 sockets — UDP datagram is sufficient for TFTP.

## 2. Module layout

New code under `crates/xboot-core/src/net/tftp/`:

| File | Purpose |
|------|---------|
| `tftp/packet.rs` | **Pure codec** — parse RRQ/ACK/ERROR packets, encode DATA/OACK/ERROR packets. No I/O. |
| `tftp/server.rs` | **Async server** — tokio `UdpSocket` on port 69, per-transfer ephemeral sockets, timeouts, retransmissions, file serving. |
| `tftp/mod.rs` | Re-exports; wires the submodule into `net`. |

Changes to existing code:

| File | Change |
|------|--------|
| `config/model.rs` | Add `tftp_root: PathBuf` to `BootConfig` — directory from which files are served |
| `config/validate.rs` | Validate `tftp_root` exists and is a directory; validate `bios_filename` and `uefi_filename` resolve within `tftp_root` |
| `net/mod.rs` | Add `pub mod tftp;` |

## 3. Protocol data flow

### Normal transfer (no options)

```
Client                    Server (:69 → ephemeral)
  |                          |
  |--- RRQ (filename) ------>|  (to port 69)
  |                          |  open ephemeral socket
  |<-- DATA block #1 --------|  (from ephemeral)
  |--- ACK #1 -------------->|
  |<-- DATA block #2 --------|
  |--- ACK #2 -------------->|
  |            ...           |
  |<-- DATA block #N (<512B)-|  (last block signals EOF)
  |--- ACK #N -------------->|
  |                          |  close ephemeral socket
```

### Transfer with options (blksize + tsize)

```
Client                    Server (:69 → ephemeral)
  |                          |
  |--- RRQ (filename,       |
  |    blksize=1468,        |
  |    tsize=0) ----------->|  (to port 69)
  |                          |  open ephemeral socket
  |<-- OACK (blksize=1468,  |  (from ephemeral)
  |          tsize=284672) --|
  |--- ACK #0 ------------->|  (ACK 0 = client accepts OACK)
  |<-- DATA block #1 (1468B)|
  |--- ACK #1 ------------->|
  |            ...           |
```

### Retransmission

```
Client                    Server
  |                          |
  |<-- DATA block #3 --------|  start 1s timer
  |        [lost]            |
  |                          |  timeout (1s), re-send
  |<-- DATA block #3 --------|  start 2s timer
  |--- ACK #3 -------------->|
  |                          |  timer cancelled
```

### Sorcerer's Apprentice (duplicate ACK)

```
Client                    Server
  |                          |
  |<-- DATA block #5 --------|
  |--- ACK #5 -------------->|  advance to block #6
  |<-- DATA block #6 --------|
  |        [lost]            |
  |--- ACK #5 -------------->|  (client re-ACKs last received)
  |<-- DATA block #6 --------|  (re-send current block)
```

## 4. Packet codec (`tftp/packet.rs`)

### Opcodes

```rust
const RRQ: u16 = 1;
const WRQ: u16 = 2;
const DATA: u16 = 3;
const ACK: u16 = 4;
const ERROR: u16 = 5;
const OACK: u16 = 6;
```

### Parser

```rust
enum TftpPacket {
    Rrq { filename: String, mode: String, options: Vec<(String, String)> },
    Ack { block: u16 },
    Error { code: u16, message: String },
}

fn parse(buf: &[u8]) -> Result<TftpPacket, TftpParseError>;
```

- RRQ: extract filename, mode ("octet"), and any trailing options as key-value pairs.
- ACK: validate 4-byte payload, extract block number.
- ERROR: extract error code and null-terminated message.
- All other opcodes (WRQ, DATA, OACK) are not parsed — the server never receives them.

### Encoder

```rust
fn encode_data(block: u16, data: &[u8]) -> Vec<u8>;
fn encode_oack(options: &[(String, String)]) -> Vec<u8>;
fn encode_error(code: u16, message: &str) -> Vec<u8>;
```

All encoders return `Vec<u8>` — the packed bytes, ready to send over UDP.

## 5. Server (`tftp/server.rs`)

### Startup

```rust
pub struct TftpServer {
    bind: IpAddr,
    tftp_root: PathBuf,
    transfer_timeout: Duration,    // default 1s
    max_retries: u32,              // default 5
    max_blksize: u16,              // default 1468
}

impl TftpServer {
    pub fn new(bind: IpAddr, tftp_root: PathBuf) -> Self;
    pub async fn serve(&self) -> io::Result<()>;
}
```

### Request loop (port 69)

1. `UdpSocket::bind(bind:69)`.
2. Loop: `recv_from(&mut buf)`.
3. Parse the datagram as RRQ (ignore non-RRQ packets silently).
4. Spawn a task: `tokio::spawn(handle_transfer(...))`.

### Transfer handler (per-request task)

1. **Validate path**: sanitise filename → resolve against `tftp_root` → reject traversal attempts.
2. **Open file**: `tokio::fs::read(path).await` — read entire binary into `Vec<u8>`.
3. **Bind ephemeral socket**: `UdpSocket::bind(bind:0)` — OS picks a free port.
4. **Option negotiation**: if RRQ contains options, respond with OACK (blksize, tsize) and wait for ACK 0. If no options, proceed to DATA block 1.
5. **Data transfer loop**:
   - Send DATA block N (up to `blksize` bytes; last block < `blksize` signals EOF).
   - Start retransmission timer.
   - Wait for ACK N (or timeout, or duplicate ACK).
   - On ACK N: advance to block N+1. If last block was sent, close and exit.
   - On timeout: re-send DATA N, double the timer, increment retry counter. If counter exceeded → send ERROR and exit.
   - On duplicate ACK (block < N): re-send DATA N (Sorcerer's Apprentice fix).
6. **Error handling**: any error → send ERROR packet to client → close socket.

### Concurrency

Each transfer runs in its own `tokio::task`. Multiple clients can fetch files simultaneously — each gets its own ephemeral socket. The :69 socket only handles the initial RRQ, so it stays responsive.

## 6. Configuration changes

### `BootConfig` (model.rs)

```rust
pub struct BootConfig {
    pub server_ip: Ipv4Addr,
    pub bind: IpAddr,
    pub tftp_root: PathBuf,          // NEW
    pub bios_filename: String,       // relative to tftp_root
    pub uefi_filename: String,       // relative to tftp_root
    pub http_script_url: String,
}
```

### Validation (validate.rs)

- `tftp_root` must exist and be a directory.
- `bios_filename` and `uefi_filename` must resolve to files within `tftp_root` (no traversal).

### TOML example

```toml
[boot]
server_ip        = "192.168.1.10"
bind             = "0.0.0.0"
tftp_root        = "/srv/xboot/tftp"
bios_filename    = "undionly.kpxe"
uefi_filename    = "ipxe.efi"
http_script_url  = "http://192.168.1.10:8080/boot.ipxe"
```

## 7. Error handling

| Condition | TFTP error | Behaviour |
|-----------|------------|-----------|
| File not found / outside root | ERROR code 1 (File not found) | Log warning, close socket |
| WRQ received | ERROR code 2 (Access violation) | Log info, close socket |
| Invalid/malformed packet | None (silent) | Log debug, ignore datagram |
| Retransmission exhausted | ERROR code 0 (Not defined) | Log warning, close socket |
| File read I/O error | ERROR code 0 (Not defined) | Log error, close socket |
| Unknown opcode | None (silent) | Log debug, ignore |

Key principle: never panic on network input. Every error path returns a proper TFTP ERROR or silent ignore.

## 8. Testing

### Unit tests — codec (`tftp/packet.rs`)

- Parse RRQ: filename + mode ("octet"), no options.
- Parse RRQ with options: blksize, tsize, timeout.
- Parse ACK (valid block numbers, edge cases like block 0 and 65535).
- Parse ERROR (various codes, null-terminated message).
- Encode DATA (block 0, block 65535, empty last block, exact-512 last block).
- Encode OACK with various option combinations.
- Encode ERROR.
- Reject: truncated packets, wrong opcodes, non-null-terminated strings, non-ASCII filenames.

### Unit tests — server (`tftp/server.rs`)

- Full transfer: RRQ → DATA/ACK cycle → EOF → close.
- Transfer with options: RRQ(blksize, tsize) → OACK → ACK 0 → DATA/ACK cycle.
- File not found → ERROR code 1.
- WRQ → ERROR code 2 (access violation).
- Path traversal: `../secret`, `/etc/passwd`, absolute paths → ERROR code 1.
- Retransmission: drop ACK → server re-sends after timeout → exponential backoff works.
- Sorcerer's Apprentice: duplicate ACK → server re-sends current block, doesn't advance.
- Multiple concurrent transfers (loopback, different ephemeral ports).
- Non-RRQ packets on port 69 → silently ignored.

### Fuzz target

- `cargo-fuzz` target: `tftp_decode` — feeds arbitrary bytes to RRQ parser; must never panic.

### Integration tests

- Fake TFTP client (in `tftp/server.rs` `#[cfg(test)]`): loopback UDP, requests a real tempfile, verifies received data matches.
- Full chain test (future, phase 07): proxyDHCP → TFTP → client receives expected binary.

## 9. Design boundary

The pure codec (`packet.rs`) is separated from the async I/O (`server.rs`). The codec is fully unit-testable without any sockets. The server is testable over real loopback UDP — same pattern proven in proxyDHCP (06a) and iSCSI transport (05c).
