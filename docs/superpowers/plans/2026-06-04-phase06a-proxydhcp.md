# Phase 06a — proxyDHCP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the proxyDHCP service that answers the PXE part of DHCP (never assigns IPs), pointing firmware PXE at the iPXE binary over TFTP and pointing iPXE at the HTTP boot script — the two-stage chainload.

**Architecture:** A pure decision/codec core (`packet` parse+encode, `options` accessors, `decide` branching) separated from a thin async I/O layer (`server`) built on `tokio::net::UdpSocket`. Two listeners — `:67` (proxy OFFER) and `:4011` (PXE Boot Server ACK) — share the core. Config grows an optional `[boot]` section; when absent the server is never started.

**Tech Stack:** Rust, tokio (`net`, `rt`, `macros`, `time`), serde/toml, thiserror, proptest, cargo-fuzz. Tests run over real loopback UDP, mirroring phase 05c's loopback-TCP transport tests.

Spec: `docs/superpowers/specs/2026-06-04-phase06a-proxydhcp-design.md`

---

## File structure

| File | Responsibility |
|------|----------------|
| `crates/xboot-core/src/config/model.rs` (modify) | Add `BootConfig` struct + `Config.boot: Option<BootConfig>` |
| `crates/xboot-core/src/config/validate.rs` (modify) | Validate `[boot]` (non-empty filenames, http(s) URL) |
| `crates/xboot-core/src/config/mod.rs` (modify) | Re-export `BootConfig` |
| `crates/xboot-core/src/net/mod.rs` (modify) | `pub mod dhcp;` |
| `crates/xboot-core/src/net/dhcp/mod.rs` (create) | Submodule wiring / re-exports |
| `crates/xboot-core/src/net/dhcp/packet.rs` (create) | BOOTP/DHCP parse + encode (pure) |
| `crates/xboot-core/src/net/dhcp/options.rs` (create) | Option-code constants, typed accessors, PXE opt-43 builders |
| `crates/xboot-core/src/net/dhcp/decide.rs` (create) | Pure `plan()` decision: iPXE-vs-firmware, BIOS-vs-UEFI |
| `crates/xboot-core/src/net/dhcp/server.rs` (create) | tokio UDP listeners, reply assembly, loopback tests |
| `crates/xboot-core/Cargo.toml` (modify) | Add tokio `time` feature (test timeouts) |
| `fuzz/fuzz_targets/dhcp_decode.rs` (create) | Fuzz the packet parser |
| `fuzz/Cargo.toml` (modify) | Register `dhcp_decode` bin |

---

## Task 1: `BootConfig` config struct

**Files:**
- Modify: `crates/xboot-core/src/config/model.rs`
- Modify: `crates/xboot-core/src/config/mod.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block at the bottom of `crates/xboot-core/src/config/model.rs`:

```rust
    #[test]
    fn parses_boot_section() {
        use std::net::{IpAddr, Ipv4Addr};
        let cfg: Config = toml::from_str(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        )
        .unwrap();
        let boot = cfg.boot.expect("boot section present");
        assert_eq!(boot.server_ip, Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(boot.bind, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(boot.bios_filename, "undionly.kpxe");
        assert_eq!(boot.uefi_filename, "ipxe.efi");
        assert_eq!(boot.http_script_url, "http://192.168.1.10/boot.ipxe");
    }

    #[test]
    fn boot_section_is_optional() {
        let cfg: Config = toml::from_str(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
"#,
        )
        .unwrap();
        assert!(cfg.boot.is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xboot-core config::model::tests::parses_boot_section`
Expected: FAIL — `Config` has no field `boot` / `BootConfig` not found.

- [ ] **Step 3: Write minimal implementation**

At the top of `crates/xboot-core/src/config/model.rs`, add to the imports:

```rust
use std::net::{IpAddr, Ipv4Addr};
```

(Keep the existing `use serde::Deserialize;` and `use crate::config::ByteSize;`.)

Add the new struct just above `pub struct Config`:

```rust
/// Optional `[boot]` section: enables the proxyDHCP / PXE network-boot service.
/// When absent, the DHCP server is not started.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BootConfig {
    /// Our IP — used for next-server (siaddr / option 66) and server-id (option 54).
    pub server_ip: Ipv4Addr,
    /// Listen address for the `:67` and `:4011` UDP sockets.
    pub bind: IpAddr,
    /// TFTP file for legacy BIOS (arch 0x0000), served by 06b.
    pub bios_filename: String,
    /// TFTP file for UEFI x64 (arch 0x0007/0x0009), served by 06b.
    pub uefi_filename: String,
    /// iPXE arm HTTP boot script; `?mac=...` is appended at runtime. Served by 06c.
    pub http_script_url: String,
}
```

Add the field to `Config` (after `client_defaults`):

```rust
    #[serde(default)]
    pub boot: Option<BootConfig>,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xboot-core config::model`
Expected: PASS (all model tests, including the two new ones).

- [ ] **Step 5: Re-export `BootConfig`**

In `crates/xboot-core/src/config/mod.rs`, extend the model re-export line:

```rust
pub use model::{
    BootConfig, Client, ClientDefaults, Config, Disk, DiskMode, DiskType, WritebackPolicy,
};
```

- [ ] **Step 6: Run the full config suite + clippy**

Run: `cargo test -p xboot-core config && cargo clippy -p xboot-core --all-targets`
Expected: PASS, zero warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/config/model.rs crates/xboot-core/src/config/mod.rs
git commit -m "feat(config): optional [boot] section (BootConfig)"
```

---

## Task 2: `[boot]` validation

**Files:**
- Modify: `crates/xboot-core/src/config/validate.rs`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/xboot-core/src/config/validate.rs`:

```rust
    #[test]
    fn accepts_valid_boot_section() {
        let cfg = parse(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        );
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn rejects_non_http_boot_url() {
        let cfg = parse(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "tftp://192.168.1.10/boot.ipxe"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::InvalidHttpUrl(u) if u.starts_with("tftp://"))));
    }

    #[test]
    fn rejects_empty_boot_filename() {
        let cfg = parse(
            r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
bios_filename   = ""
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::EmptyBootField { field } if field == "bios_filename")));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xboot-core config::validate::tests::rejects_non_http_boot_url`
Expected: FAIL — `ValidationError::InvalidHttpUrl` does not exist.

- [ ] **Step 3: Write minimal implementation**

In `crates/xboot-core/src/config/validate.rs`, add two variants to the `ValidationError` enum (after `DuplicateMac`):

```rust
    #[error("boot.http_script_url must start with http:// or https://: {0}")]
    InvalidHttpUrl(String),
    #[error("boot.{field} must not be empty")]
    EmptyBootField { field: String },
```

Add this free function above `pub fn validate`:

```rust
fn check_boot(errors: &mut Vec<ValidationError>, boot: &crate::config::BootConfig) {
    if boot.bios_filename.is_empty() {
        errors.push(ValidationError::EmptyBootField {
            field: "bios_filename".to_string(),
        });
    }
    if boot.uefi_filename.is_empty() {
        errors.push(ValidationError::EmptyBootField {
            field: "uefi_filename".to_string(),
        });
    }
    let url = &boot.http_script_url;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        errors.push(ValidationError::InvalidHttpUrl(url.clone()));
    }
}
```

Inside `pub fn validate`, just before the final `if errors.is_empty()` block, call it:

```rust
    if let Some(boot) = &cfg.boot {
        check_boot(&mut errors, boot);
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xboot-core config::validate`
Expected: PASS.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p xboot-core --all-targets`
Expected: zero warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/xboot-core/src/config/validate.rs
git commit -m "feat(config): validate [boot] filenames and http(s) URL"
```

---

## Task 3: DHCP/BOOTP packet codec

**Files:**
- Modify: `crates/xboot-core/src/net/mod.rs`
- Create: `crates/xboot-core/src/net/dhcp/mod.rs`
- Create: `crates/xboot-core/src/net/dhcp/packet.rs`

- [ ] **Step 1: Wire the module in**

In `crates/xboot-core/src/net/mod.rs`, add at the top (above the existing `use std::io;`):

```rust
pub mod dhcp;
```

Create `crates/xboot-core/src/net/dhcp/mod.rs`:

```rust
//! proxyDHCP — answers only the PXE part of DHCP; never assigns IP addresses
//! (phase 06a). Pure codec/decision core (`packet`, `options`, `decide`) +
//! thin async I/O (`server`).

pub mod packet;
```

- [ ] **Step 2: Write the failing test**

Create `crates/xboot-core/src/net/dhcp/packet.rs` with only the test module first (implementation lands in Step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sample() -> DhcpMessage {
        DhcpMessage {
            op: BOOTREQUEST,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 0xDEAD_BEEF,
            secs: 0,
            flags: 0x8000,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::new(10, 0, 0, 1),
            chaddr: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            options: vec![
                DhcpOption { code: 53, data: vec![1] },
                DhcpOption { code: 60, data: b"PXEClient".to_vec() },
            ],
        }
    }

    #[test]
    fn round_trip_message() {
        let msg = sample();
        let bytes = encode(&msg);
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn rejects_short_packet() {
        assert_eq!(parse(&[0u8; 10]), Err(ParseError::TooShort(10)));
    }

    #[test]
    fn rejects_bad_cookie() {
        let mut bytes = encode(&sample());
        bytes[236] = 0; // corrupt the magic cookie
        assert_eq!(parse(&bytes), Err(ParseError::BadCookie));
    }

    #[test]
    fn rejects_truncated_option() {
        let mut bytes = encode(&sample());
        // Append an option claiming 5 bytes but supplying none.
        bytes.truncate(240);
        bytes.extend_from_slice(&[99, 5]); // code 99, len 5, no data
        assert_eq!(parse(&bytes), Err(ParseError::TruncatedOption { code: 99 }));
    }

    #[test]
    fn skips_pad_and_stops_at_end() {
        let mut bytes = vec![0u8; 240];
        bytes[0] = BOOTREQUEST;
        bytes[236..240].copy_from_slice(&MAGIC_COOKIE);
        bytes.extend_from_slice(&[0, 0, 53, 1, 1, 255, 0, 0]); // PADs, opt53=DISCOVER, END, trailing
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.options, vec![DhcpOption { code: 53, data: vec![1] }]);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p xboot-core net::dhcp::packet`
Expected: FAIL to compile — `DhcpMessage`, `parse`, `encode` not defined.

- [ ] **Step 4: Write minimal implementation**

Prepend the implementation above the test module in `crates/xboot-core/src/net/dhcp/packet.rs`:

```rust
//! BOOTP/DHCP packet parse + encode. Pure, no I/O. The parser returns `Result`
//! and never panics on malformed input.

use std::net::Ipv4Addr;

/// DHCP magic cookie (RFC 2131) that precedes the options area.
pub const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// BOOTP `op` for a client request.
pub const BOOTREQUEST: u8 = 1;
/// BOOTP `op` for a server reply.
pub const BOOTREPLY: u8 = 2;

/// Offset of the magic cookie; the fixed BOOTP header is `[0, 236)`.
const COOKIE_OFFSET: usize = 236;
/// Minimum valid packet: fixed header + cookie.
const MIN_LEN: usize = 240;

/// A parsed BOOTP/DHCP message. The options area is kept as raw TLVs; typed
/// access lives in `options.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpMessage {
    pub op: u8,
    pub htype: u8,
    pub hlen: u8,
    pub hops: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub options: Vec<DhcpOption>,
}

/// One DHCP option as a code + raw value (TLV). PAD (0) and END (255) are not
/// stored; END terminates parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpOption {
    pub code: u8,
    pub data: Vec<u8>,
}

/// Error returned by [`parse`]. All variants are recoverable: the caller drops
/// the datagram and keeps serving.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("packet too short: {0} bytes")]
    TooShort(usize),
    #[error("bad magic cookie")]
    BadCookie,
    #[error("truncated option (code {code})")]
    TruncatedOption { code: u8 },
}

fn ipv4(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

/// Parse a datagram into a [`DhcpMessage`]. Never panics.
pub fn parse(buf: &[u8]) -> Result<DhcpMessage, ParseError> {
    if buf.len() < MIN_LEN {
        return Err(ParseError::TooShort(buf.len()));
    }
    if buf[COOKIE_OFFSET..MIN_LEN] != MAGIC_COOKIE {
        return Err(ParseError::BadCookie);
    }
    let mut chaddr = [0u8; 16];
    chaddr.copy_from_slice(&buf[28..44]);
    Ok(DhcpMessage {
        op: buf[0],
        htype: buf[1],
        hlen: buf[2],
        hops: buf[3],
        xid: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        secs: u16::from_be_bytes([buf[8], buf[9]]),
        flags: u16::from_be_bytes([buf[10], buf[11]]),
        ciaddr: ipv4(&buf[12..16]),
        yiaddr: ipv4(&buf[16..20]),
        siaddr: ipv4(&buf[20..24]),
        giaddr: ipv4(&buf[24..28]),
        chaddr,
        options: parse_options(&buf[MIN_LEN..])?,
    })
}

fn parse_options(buf: &[u8]) -> Result<Vec<DhcpOption>, ParseError> {
    let mut opts = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let code = buf[i];
        match code {
            0 => {
                i += 1;
                continue;
            } // PAD
            255 => break, // END
            _ => {}
        }
        if i + 1 >= buf.len() {
            return Err(ParseError::TruncatedOption { code });
        }
        let len = buf[i + 1] as usize;
        let start = i + 2;
        let end = start + len;
        if end > buf.len() {
            return Err(ParseError::TruncatedOption { code });
        }
        opts.push(DhcpOption {
            code,
            data: buf[start..end].to_vec(),
        });
        i = end;
    }
    Ok(opts)
}

/// Encode a [`DhcpMessage`] into wire bytes (fixed header + cookie + TLV options + END).
pub fn encode(msg: &DhcpMessage) -> Vec<u8> {
    let mut out = vec![0u8; MIN_LEN];
    out[0] = msg.op;
    out[1] = msg.htype;
    out[2] = msg.hlen;
    out[3] = msg.hops;
    out[4..8].copy_from_slice(&msg.xid.to_be_bytes());
    out[8..10].copy_from_slice(&msg.secs.to_be_bytes());
    out[10..12].copy_from_slice(&msg.flags.to_be_bytes());
    out[12..16].copy_from_slice(&msg.ciaddr.octets());
    out[16..20].copy_from_slice(&msg.yiaddr.octets());
    out[20..24].copy_from_slice(&msg.siaddr.octets());
    out[24..28].copy_from_slice(&msg.giaddr.octets());
    out[28..44].copy_from_slice(&msg.chaddr);
    out[COOKIE_OFFSET..MIN_LEN].copy_from_slice(&MAGIC_COOKIE);
    for opt in &msg.options {
        out.push(opt.code);
        out.push(opt.data.len() as u8);
        out.extend_from_slice(&opt.data);
    }
    out.push(255); // END
    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p xboot-core net::dhcp::packet`
Expected: PASS (5 tests).

- [ ] **Step 6: Add the no-panic proptest**

Append to the `tests` module (inside it, after the existing tests):

```rust
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
            let _ = parse(&bytes);
        }
    }
```

- [ ] **Step 7: Run the proptest + clippy**

Run: `cargo test -p xboot-core net::dhcp::packet && cargo clippy -p xboot-core --all-targets`
Expected: PASS, zero warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/xboot-core/src/net/mod.rs crates/xboot-core/src/net/dhcp/mod.rs crates/xboot-core/src/net/dhcp/packet.rs
git commit -m "feat(dhcp): BOOTP/DHCP packet parse + encode codec"
```

---

## Task 4: Option constants, typed accessors, PXE opt-43 builders

**Files:**
- Create: `crates/xboot-core/src/net/dhcp/options.rs`
- Modify: `crates/xboot-core/src/net/dhcp/mod.rs`

- [ ] **Step 1: Wire the module in**

In `crates/xboot-core/src/net/dhcp/mod.rs`, add below `pub mod packet;`:

```rust
pub mod options;
```

- [ ] **Step 2: Write the failing test**

Create `crates/xboot-core/src/net/dhcp/options.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::dhcp::packet::{DhcpMessage, DhcpOption};
    use std::net::Ipv4Addr;

    fn msg(options: Vec<DhcpOption>) -> DhcpMessage {
        DhcpMessage {
            op: 1,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 1,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [0; 16],
            options,
        }
    }

    #[test]
    fn typed_accessors() {
        let m = msg(vec![
            DhcpOption { code: MSG_TYPE, data: vec![msg_type::DISCOVER] },
            DhcpOption { code: VENDOR_CLASS_ID, data: b"PXEClient:Arch:00007".to_vec() },
            DhcpOption { code: CLIENT_SYSTEM_ARCH, data: vec![0x00, 0x07] },
            DhcpOption { code: USER_CLASS, data: b"iPXE".to_vec() },
        ]);
        assert_eq!(m.message_type(), Some(msg_type::DISCOVER));
        assert_eq!(m.vendor_class(), Some(b"PXEClient:Arch:00007".as_ref()));
        assert_eq!(m.client_arch(), Some(0x0007));
        assert_eq!(m.user_class(), Some(b"iPXE".as_ref()));
    }

    #[test]
    fn missing_and_short_options_are_none() {
        let m = msg(vec![DhcpOption { code: CLIENT_SYSTEM_ARCH, data: vec![0x00] }]);
        assert_eq!(m.message_type(), None);
        assert_eq!(m.client_arch(), None); // 1 byte is too short for u16
    }

    #[test]
    fn pxe_offer_vendor_opts_layout() {
        let v = pxe_offer_vendor_opts(Ipv4Addr::new(192, 168, 1, 10));
        // sub-opt 6 (discovery control), len 1, value 0x07
        assert_eq!(&v[0..3], &[pxe::DISCOVERY_CONTROL, 1, 0x07]);
        // sub-opt 8 (boot servers), len 7, type 0x0000, count 1, IP
        assert_eq!(v[3], pxe::BOOT_SERVERS);
        assert_eq!(v[4], 7);
        assert_eq!(&v[5..7], &[0x00, 0x00]);
        assert_eq!(v[7], 1);
        assert_eq!(&v[8..12], &[192, 168, 1, 10]);
        assert_eq!(*v.last().unwrap(), pxe::END);
    }

    #[test]
    fn pxe_ack_vendor_opts_layout() {
        let v = pxe_ack_vendor_opts();
        // sub-opt 71 (boot item), len 4, type 0x0000, layer 0x0000
        assert_eq!(&v[0..6], &[pxe::BOOT_ITEM, 4, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(*v.last().unwrap(), pxe::END);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p xboot-core net::dhcp::options`
Expected: FAIL to compile — constants, accessors, builders not defined.

- [ ] **Step 4: Write minimal implementation**

Prepend above the test module in `crates/xboot-core/src/net/dhcp/options.rs`:

```rust
//! DHCP option-code constants, typed accessors on `DhcpMessage`, and PXE
//! option-43 (vendor-specific) sub-option builders.

use std::net::Ipv4Addr;

use crate::net::dhcp::packet::DhcpMessage;

// --- DHCP option codes ---
pub const MSG_TYPE: u8 = 53;
pub const PARAM_REQUEST_LIST: u8 = 55;
pub const SERVER_ID: u8 = 54;
pub const TFTP_SERVER_NAME: u8 = 66;
pub const BOOTFILE_NAME: u8 = 67;
pub const VENDOR_CLASS_ID: u8 = 60;
pub const USER_CLASS: u8 = 77;
pub const CLIENT_SYSTEM_ARCH: u8 = 93;
pub const CLIENT_MACHINE_ID: u8 = 97;
pub const VENDOR_SPECIFIC: u8 = 43;

/// DHCP message types (option 53 values).
pub mod msg_type {
    pub const DISCOVER: u8 = 1;
    pub const OFFER: u8 = 2;
    pub const REQUEST: u8 = 3;
    pub const ACK: u8 = 5;
}

/// PXE option-43 sub-option codes (Intel PXE spec).
pub mod pxe {
    pub const DISCOVERY_CONTROL: u8 = 6;
    pub const BOOT_SERVERS: u8 = 8;
    pub const BOOT_ITEM: u8 = 71;
    pub const END: u8 = 255;
}

impl DhcpMessage {
    /// Raw value of the first option with `code`, if present.
    pub fn option(&self, code: u8) -> Option<&[u8]> {
        self.options
            .iter()
            .find(|o| o.code == code)
            .map(|o| o.data.as_slice())
    }

    /// Option 53 message type.
    pub fn message_type(&self) -> Option<u8> {
        self.option(MSG_TYPE).and_then(|d| d.first().copied())
    }

    /// Option 60 vendor class identifier.
    pub fn vendor_class(&self) -> Option<&[u8]> {
        self.option(VENDOR_CLASS_ID)
    }

    /// Option 93 client system architecture (first 2 bytes, big-endian).
    pub fn client_arch(&self) -> Option<u16> {
        self.option(CLIENT_SYSTEM_ARCH)
            .filter(|d| d.len() >= 2)
            .map(|d| u16::from_be_bytes([d[0], d[1]]))
    }

    /// Option 77 user class.
    pub fn user_class(&self) -> Option<&[u8]> {
        self.option(USER_CLASS)
    }
}

/// Option-43 payload for the `:67` proxy OFFER: discovery control + a single
/// boot-server entry pointing the client at our `:4011` service.
pub fn pxe_offer_vendor_opts(server_ip: Ipv4Addr) -> Vec<u8> {
    let mut v = Vec::new();
    // PXE_DISCOVERY_CONTROL = disable broadcast + multicast, use boot-server list only.
    v.extend_from_slice(&[pxe::DISCOVERY_CONTROL, 1, 0x07]);
    // PXE_BOOT_SERVERS: one entry — server type 0x0000, IP count 1, our IP.
    let mut bs = Vec::new();
    bs.extend_from_slice(&[0x00, 0x00]); // boot server type
    bs.push(1); // IP count
    bs.extend_from_slice(&server_ip.octets());
    v.push(pxe::BOOT_SERVERS);
    v.push(bs.len() as u8);
    v.extend_from_slice(&bs);
    v.push(pxe::END);
    v
}

/// Option-43 payload for the `:4011` ACK: a single boot item (type 0, layer 0).
pub fn pxe_ack_vendor_opts() -> Vec<u8> {
    let mut v = Vec::new();
    v.push(pxe::BOOT_ITEM);
    v.push(4);
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // boot item type + layer
    v.push(pxe::END);
    v
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p xboot-core net::dhcp::options`
Expected: PASS (4 tests).

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p xboot-core --all-targets`
Expected: zero warnings. (`PARAM_REQUEST_LIST` and `CLIENT_MACHINE_ID` are public consts — no dead-code warning for `pub` items.)

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/net/dhcp/options.rs crates/xboot-core/src/net/dhcp/mod.rs
git commit -m "feat(dhcp): option constants, typed accessors, PXE opt-43 builders"
```

---

## Task 5: Pure decision function `decide::plan`

**Files:**
- Create: `crates/xboot-core/src/net/dhcp/decide.rs`
- Modify: `crates/xboot-core/src/net/dhcp/mod.rs`

- [ ] **Step 1: Wire the module in**

In `crates/xboot-core/src/net/dhcp/mod.rs`, add below `pub mod options;`:

```rust
pub mod decide;
```

- [ ] **Step 2: Write the failing test**

Create `crates/xboot-core/src/net/dhcp/decide.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BootConfig;
    use crate::net::dhcp::options::{CLIENT_SYSTEM_ARCH, USER_CLASS};
    use crate::net::dhcp::packet::{DhcpMessage, DhcpOption};
    use std::net::{IpAddr, Ipv4Addr};

    fn cfg() -> BootConfig {
        BootConfig {
            server_ip: Ipv4Addr::new(192, 168, 1, 10),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bios_filename: "undionly.kpxe".to_string(),
            uefi_filename: "ipxe.efi".to_string(),
            http_script_url: "http://192.168.1.10/boot.ipxe".to_string(),
        }
    }

    fn msg(options: Vec<DhcpOption>) -> DhcpMessage {
        DhcpMessage {
            op: 1,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 1,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            options,
        }
    }

    #[test]
    fn bios_arch_gets_bios_filename() {
        let m = msg(vec![DhcpOption { code: CLIENT_SYSTEM_ARCH, data: vec![0x00, 0x00] }]);
        let plan = plan(&m, &cfg()).unwrap();
        assert_eq!(plan.bootfile, "undionly.kpxe");
        assert_eq!(plan.next_server, Some(Ipv4Addr::new(192, 168, 1, 10)));
    }

    #[test]
    fn uefi_arch_gets_uefi_filename() {
        for arch in [[0x00, 0x07], [0x00, 0x09]] {
            let m = msg(vec![DhcpOption { code: CLIENT_SYSTEM_ARCH, data: arch.to_vec() }]);
            let plan = plan(&m, &cfg()).unwrap();
            assert_eq!(plan.bootfile, "ipxe.efi");
            assert_eq!(plan.next_server, Some(Ipv4Addr::new(192, 168, 1, 10)));
        }
    }

    #[test]
    fn ipxe_user_class_gets_http_url_with_mac() {
        let m = msg(vec![
            DhcpOption { code: USER_CLASS, data: b"iPXE".to_vec() },
            DhcpOption { code: CLIENT_SYSTEM_ARCH, data: vec![0x00, 0x07] },
        ]);
        let plan = plan(&m, &cfg()).unwrap();
        assert_eq!(plan.bootfile, "http://192.168.1.10/boot.ipxe?mac=aa:bb:cc:dd:ee:01");
        assert_eq!(plan.next_server, None);
    }

    #[test]
    fn unknown_arch_is_none() {
        let m = msg(vec![DhcpOption { code: CLIENT_SYSTEM_ARCH, data: vec![0x00, 0x42] }]);
        assert_eq!(plan(&m, &cfg()), None);
    }

    #[test]
    fn no_arch_no_ipxe_is_none() {
        let m = msg(vec![]);
        assert_eq!(plan(&m, &cfg()), None);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p xboot-core net::dhcp::decide`
Expected: FAIL to compile — `plan`, `OfferPlan` not defined.

- [ ] **Step 4: Write minimal implementation**

Prepend above the test module in `crates/xboot-core/src/net/dhcp/decide.rs`:

```rust
//! Pure decision core: given a parsed request + boot config, decide what to
//! offer. No sockets, fully unit-testable. Returns `None` to stay silent.

use std::net::Ipv4Addr;

use crate::config::BootConfig;
use crate::net::dhcp::packet::DhcpMessage;

/// Client system architecture (option 93) codes we support.
pub const ARCH_BIOS_X86: u16 = 0x0000;
pub const ARCH_UEFI_X64: u16 = 0x0007;
pub const ARCH_UEFI_X64_ALT: u16 = 0x0009;

/// What to put in the reply: the bootfile (option 67) and, on the firmware arm,
/// the next-server IP (siaddr / option 66). `None` next-server = iPXE HTTP arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferPlan {
    pub bootfile: String,
    pub next_server: Option<Ipv4Addr>,
}

/// Decide the boot plan for a request. `None` means "stay silent".
pub fn plan(msg: &DhcpMessage, cfg: &BootConfig) -> Option<OfferPlan> {
    // iPXE arm — option 77 user class contains "iPXE". Breaks the chainload loop
    // by handing iPXE the HTTP script instead of the iPXE binary again.
    if is_ipxe(msg) {
        let mac = format_mac(&msg.chaddr[..(msg.hlen as usize).min(16)]);
        return Some(OfferPlan {
            bootfile: format!("{}?mac={}", cfg.http_script_url, mac),
            next_server: None,
        });
    }

    // Firmware arm — branch on architecture (option 93).
    match msg.client_arch()? {
        ARCH_BIOS_X86 => Some(OfferPlan {
            bootfile: cfg.bios_filename.clone(),
            next_server: Some(cfg.server_ip),
        }),
        ARCH_UEFI_X64 | ARCH_UEFI_X64_ALT => Some(OfferPlan {
            bootfile: cfg.uefi_filename.clone(),
            next_server: Some(cfg.server_ip),
        }),
        _ => None,
    }
}

/// True if the user-class option carries "iPXE" (plain or RFC-3004 length-prefixed).
fn is_ipxe(msg: &DhcpMessage) -> bool {
    matches!(msg.user_class(), Some(uc) if uc.windows(4).any(|w| w == b"iPXE"))
}

/// Lower-case colon-separated MAC from the leading hardware-address bytes.
fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p xboot-core net::dhcp::decide`
Expected: PASS (5 tests).

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -p xboot-core --all-targets`
Expected: zero warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/net/dhcp/decide.rs crates/xboot-core/src/net/dhcp/mod.rs
git commit -m "feat(dhcp): pure decision core (iPXE vs firmware, BIOS vs UEFI)"
```

---

## Task 6: Async UDP server + reply assembly + loopback tests

**Files:**
- Modify: `crates/xboot-core/Cargo.toml`
- Create: `crates/xboot-core/src/net/dhcp/server.rs`
- Modify: `crates/xboot-core/src/net/dhcp/mod.rs`

- [ ] **Step 1: Add the tokio `time` feature (for test timeouts)**

In `crates/xboot-core/Cargo.toml`, change the tokio dependency line to add `"time"`:

```toml
tokio = { workspace = true, features = ["net", "io-util", "rt", "time"] }
```

- [ ] **Step 2: Wire the module in**

In `crates/xboot-core/src/net/dhcp/mod.rs`, add below `pub mod decide;`:

```rust
pub mod server;
```

- [ ] **Step 3: Write the failing test**

Create `crates/xboot-core/src/net/dhcp/server.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BootConfig;
    use crate::net::dhcp::options::{self, msg_type};
    use crate::net::dhcp::packet::{self, DhcpMessage, DhcpOption, BOOTREPLY};
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    fn test_cfg() -> BootConfig {
        BootConfig {
            server_ip: Ipv4Addr::new(192, 168, 1, 10),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            bios_filename: "undionly.kpxe".to_string(),
            uefi_filename: "ipxe.efi".to_string(),
            http_script_url: "http://192.168.1.10/boot.ipxe".to_string(),
        }
    }

    /// Build a DISCOVER from a client. `flags = 0` so the reply is unicast back
    /// to our ephemeral source port (makes loopback testing possible).
    fn discover(arch: Option<[u8; 2]>, ipxe: bool) -> Vec<u8> {
        let mut options = vec![
            DhcpOption { code: options::MSG_TYPE, data: vec![msg_type::DISCOVER] },
            DhcpOption { code: options::VENDOR_CLASS_ID, data: b"PXEClient".to_vec() },
        ];
        if let Some(a) = arch {
            options.push(DhcpOption { code: options::CLIENT_SYSTEM_ARCH, data: a.to_vec() });
        }
        if ipxe {
            options.push(DhcpOption { code: options::USER_CLASS, data: b"iPXE".to_vec() });
        }
        packet::encode(&DhcpMessage {
            op: 1,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid: 0x1234,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            options,
        })
    }

    #[test]
    fn build_reply_is_a_proxy_offer() {
        let cfg = test_cfg();
        let req = packet::parse(&discover(Some([0x00, 0x00]), false)).unwrap();
        let plan = crate::net::dhcp::decide::plan(&req, &cfg).unwrap();
        let reply = build_reply(
            &req,
            &plan,
            &cfg,
            msg_type::OFFER,
            options::pxe_offer_vendor_opts(cfg.server_ip),
        );
        assert_eq!(reply.op, BOOTREPLY);
        assert_eq!(reply.yiaddr, Ipv4Addr::UNSPECIFIED); // never assign an IP
        assert_eq!(reply.xid, 0x1234); // echoed
        assert_eq!(reply.siaddr, Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(reply.message_type(), Some(msg_type::OFFER));
        assert_eq!(reply.option(options::SERVER_ID), Some([192, 168, 1, 10].as_ref()));
        assert_eq!(reply.option(options::VENDOR_CLASS_ID), Some(b"PXEClient".as_ref()));
        assert_eq!(reply.option(options::BOOTFILE_NAME), Some(b"undionly.kpxe".as_ref()));
        assert_eq!(reply.option(options::TFTP_SERVER_NAME), Some(b"192.168.1.10".as_ref()));
        assert!(reply.option(options::VENDOR_SPECIFIC).is_some());
    }

    async fn recv_reply(client: &UdpSocket) -> DhcpMessage {
        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .expect("reply within timeout")
            .expect("recv ok");
        packet::parse(&buf[..n]).unwrap()
    }

    #[tokio::test]
    async fn two_stage_chainload_over_loopback() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let cfg = Arc::new(test_cfg());
        tokio::spawn(async move {
            let _ = listener_loop(sock, cfg, Role::Proxy).await;
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Stage 1: firmware DISCOVER (BIOS) → iPXE binary over TFTP.
        client.send_to(&discover(Some([0x00, 0x00]), false), addr).await.unwrap();
        let r1 = recv_reply(&client).await;
        assert_eq!(r1.option(options::BOOTFILE_NAME), Some(b"undionly.kpxe".as_ref()));
        assert_eq!(r1.siaddr, Ipv4Addr::new(192, 168, 1, 10));

        // Stage 2: iPXE re-DISCOVER → HTTP boot script (loop broken).
        client.send_to(&discover(Some([0x00, 0x07]), true), addr).await.unwrap();
        let r2 = recv_reply(&client).await;
        assert_eq!(
            r2.option(options::BOOTFILE_NAME),
            Some(b"http://192.168.1.10/boot.ipxe?mac=aa:bb:cc:dd:ee:01".as_ref())
        );
        assert_eq!(r2.siaddr, Ipv4Addr::UNSPECIFIED); // no next-server on the HTTP arm
    }

    #[tokio::test]
    async fn non_pxe_datagram_is_ignored() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let cfg = Arc::new(test_cfg());
        tokio::spawn(async move {
            let _ = listener_loop(sock, cfg, Role::Proxy).await;
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // A DISCOVER without the PXEClient vendor class → must be dropped silently.
        let mut msg = packet::parse(&discover(Some([0x00, 0x00]), false)).unwrap();
        msg.options.retain(|o| o.code != options::VENDOR_CLASS_ID);
        client.send_to(&packet::encode(&msg), addr).await.unwrap();

        let mut buf = [0u8; 2048];
        let got = tokio::time::timeout(Duration::from_millis(300), client.recv_from(&mut buf)).await;
        assert!(got.is_err(), "server must not reply to non-PXE traffic");
    }
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p xboot-core net::dhcp::server`
Expected: FAIL to compile — `build_reply`, `listener_loop`, `Role` not defined.

- [ ] **Step 5: Write minimal implementation**

Prepend above the test module in `crates/xboot-core/src/net/dhcp/server.rs`:

```rust
//! Thin async I/O layer: two UDP listeners (`:67` proxy OFFER, `:4011` PXE Boot
//! Server ACK) sharing the pure decision/codec core. Survives any single bad
//! datagram and keeps serving.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::config::BootConfig;
use crate::net::dhcp::decide::{self, OfferPlan};
use crate::net::dhcp::options::{self, msg_type};
use crate::net::dhcp::packet::{self, DhcpMessage, DhcpOption, BOOTREPLY};

/// BOOTP broadcast flag (high bit of the 16-bit flags field).
const BROADCAST_FLAG: u16 = 0x8000;

/// Which listener role a datagram arrived on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `:67` — answers DISCOVER with a proxy OFFER.
    Proxy,
    /// `:4011` — answers REQUEST with a boot-service ACK.
    BootService,
}

/// Start the proxyDHCP service: bind both listeners and serve until one errors.
pub async fn serve(cfg: BootConfig) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    let proxy = bind_socket(cfg.bind, 67).await?;
    let bootsvc = bind_socket(cfg.bind, 4011).await?;
    tokio::select! {
        r = listener_loop(proxy, cfg.clone(), Role::Proxy) => r,
        r = listener_loop(bootsvc, cfg.clone(), Role::BootService) => r,
    }
}

async fn bind_socket(bind: IpAddr, port: u16) -> std::io::Result<UdpSocket> {
    let sock = UdpSocket::bind(SocketAddr::new(bind, port)).await?;
    sock.set_broadcast(true)?;
    Ok(sock)
}

/// Receive → handle → send, forever. A bad datagram never breaks the loop.
async fn listener_loop(sock: UdpSocket, cfg: Arc<BootConfig>, role: Role) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        let (n, from) = sock.recv_from(&mut buf).await?;
        if let Some((reply, dest)) = handle_datagram(&buf[..n], &cfg, role, from) {
            sock.send_to(&packet::encode(&reply), dest).await?;
        }
    }
}

/// Pure request→reply decision (no sockets). `None` = drop silently.
fn handle_datagram(
    data: &[u8],
    cfg: &BootConfig,
    role: Role,
    from: SocketAddr,
) -> Option<(DhcpMessage, SocketAddr)> {
    let req = match packet::parse(data) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("dhcp: dropping malformed datagram ({} bytes): {e}", data.len());
            return None;
        }
    };
    if !is_pxe_request(&req, role) {
        return None;
    }
    let plan = decide::plan(&req, cfg)?;
    let (mtype, vendor) = match role {
        Role::Proxy => (msg_type::OFFER, options::pxe_offer_vendor_opts(cfg.server_ip)),
        Role::BootService => (msg_type::ACK, options::pxe_ack_vendor_opts()),
    };
    let reply = build_reply(&req, &plan, cfg, mtype, vendor);
    Some((reply, reply_dest(&req, role, from)))
}

/// Guard: only DISCOVER (proxy) / REQUEST (boot service) carrying a PXEClient
/// vendor class. Everything else is left to the network's real DHCP server.
fn is_pxe_request(req: &DhcpMessage, role: Role) -> bool {
    let want = match role {
        Role::Proxy => msg_type::DISCOVER,
        Role::BootService => msg_type::REQUEST,
    };
    if req.message_type() != Some(want) {
        return false;
    }
    matches!(req.vendor_class(), Some(v) if v.starts_with(b"PXEClient"))
}

/// Assemble a proxy reply: `yiaddr = 0` always; echo xid/chaddr/flags; carry the
/// PXE options the firmware needs to accept the offer.
fn build_reply(
    req: &DhcpMessage,
    plan: &OfferPlan,
    cfg: &BootConfig,
    mtype: u8,
    vendor43: Vec<u8>,
) -> DhcpMessage {
    let mut opts = vec![
        DhcpOption { code: options::MSG_TYPE, data: vec![mtype] },
        DhcpOption { code: options::SERVER_ID, data: cfg.server_ip.octets().to_vec() },
        DhcpOption { code: options::VENDOR_CLASS_ID, data: b"PXEClient".to_vec() },
        DhcpOption { code: options::BOOTFILE_NAME, data: plan.bootfile.clone().into_bytes() },
    ];
    if let Some(ns) = plan.next_server {
        // Option 66 as the ASCII dotted IP (dnsmasq-compatible) + siaddr below.
        opts.push(DhcpOption {
            code: options::TFTP_SERVER_NAME,
            data: ns.to_string().into_bytes(),
        });
    }
    opts.push(DhcpOption { code: options::VENDOR_SPECIFIC, data: vendor43 });

    DhcpMessage {
        op: BOOTREPLY,
        htype: req.htype,
        hlen: req.hlen,
        hops: 0,
        xid: req.xid,
        secs: 0,
        flags: req.flags,
        ciaddr: Ipv4Addr::UNSPECIFIED,
        yiaddr: Ipv4Addr::UNSPECIFIED,
        siaddr: plan.next_server.unwrap_or(Ipv4Addr::UNSPECIFIED),
        giaddr: req.giaddr,
        chaddr: req.chaddr,
        options: opts,
    }
}

/// Where to send the reply: relay agent (giaddr) → unicast to giaddr:67;
/// broadcast-flagged proxy offer → 255.255.255.255:68; otherwise unicast back
/// to the sender (covers the `:4011` path and loopback tests).
fn reply_dest(req: &DhcpMessage, role: Role, from: SocketAddr) -> SocketAddr {
    if req.giaddr != Ipv4Addr::UNSPECIFIED {
        SocketAddr::new(IpAddr::V4(req.giaddr), 67)
    } else if role == Role::Proxy && (req.flags & BROADCAST_FLAG) != 0 {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 68)
    } else {
        from
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p xboot-core net::dhcp::server`
Expected: PASS (3 tests: `build_reply_is_a_proxy_offer`, `two_stage_chainload_over_loopback`, `non_pxe_datagram_is_ignored`).

- [ ] **Step 7: Run the full core suite + clippy + fmt**

Run: `cargo test -p xboot-core && cargo clippy -p xboot-core --all-targets && cargo fmt --check`
Expected: all PASS, zero warnings. (If `fmt --check` reports diffs, run `cargo fmt` and re-check.)

- [ ] **Step 8: Commit**

```bash
git add crates/xboot-core/Cargo.toml crates/xboot-core/src/net/dhcp/server.rs crates/xboot-core/src/net/dhcp/mod.rs
git commit -m "feat(dhcp): tokio UDP listeners, proxy-offer assembly, loopback tests"
```

---

## Task 7: cargo-fuzz target for the packet parser

**Files:**
- Create: `fuzz/fuzz_targets/dhcp_decode.rs`
- Modify: `fuzz/Cargo.toml`

- [ ] **Step 1: Create the fuzz target**

Create `fuzz/fuzz_targets/dhcp_decode.rs` (mirrors `iscsi_pdu.rs`):

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The DHCP packet parser must never panic, regardless of input.
    let _ = xboot_core::net::dhcp::packet::parse(data);
});
```

- [ ] **Step 2: Register the binary**

Append to `fuzz/Cargo.toml`:

```toml
[[bin]]
name = "dhcp_decode"
path = "fuzz_targets/dhcp_decode.rs"
test = false
doc = false
```

- [ ] **Step 3: Verify it builds**

Run: `cargo +nightly fuzz build dhcp_decode`
Expected: builds successfully. (If a nightly toolchain or cargo-fuzz is unavailable in CI, instead run `cargo build --manifest-path fuzz/Cargo.toml --bin dhcp_decode` to confirm the target compiles. Note either fallback used.)

- [ ] **Step 4: Smoke-run the fuzzer briefly (optional, if nightly available)**

Run: `cargo +nightly fuzz run dhcp_decode -- -max_total_time=30`
Expected: no crashes in 30 seconds.

- [ ] **Step 5: Commit**

```bash
git add fuzz/fuzz_targets/dhcp_decode.rs fuzz/Cargo.toml
git commit -m "test(dhcp): cargo-fuzz target for the packet parser"
```

---

## Final verification

- [ ] **Run the complete workspace gate**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all tests pass, zero clippy warnings, formatting clean.

- [ ] **Confirm the [boot]-absent path still works**

The existing iSCSI-only and config tests must be unaffected (the `[boot]` section is `Option`, defaulting to `None`). Confirmed by the passing `boot_section_is_optional` test (Task 1) and the unchanged existing suite.

---

## Notes / out of scope (per spec §1)

- **Binary wiring** (`crates/xboot/src/main.rs` calling `dhcp::server::serve`) is intentionally **not** included here, matching the iSCSI transport which is also not yet wired into `main`. `serve()` is the ready entry point for whenever the binary's run-loop is assembled.
- IP assignment, raw L2 packets, interactive PXE menus, and architectures beyond UEFI x64 / legacy BIOS remain out of scope (spec §1).
- The exact option-43 sub-option *values* (discovery control `0x07`, boot-item type/layer) are encoded as a concrete minimal set; final firmware acceptance is validated in the manual Windows-VM E2E run (master spec §7), not in unit tests, which assert TLV structure only.
```
