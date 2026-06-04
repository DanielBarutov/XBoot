# Phase 06a — proxyDHCP design

Part of the **Network boot** subsystem (master spec
`docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md`, §3.1, §4).
Phase 06 is split into three sub-phases, each with its own design → plan → implement cycle:

- **06a — proxyDHCP** (this document)
- 06b — TFTP (serves the prebuilt iPXE binaries)
- 06c — HTTP (serves the per-MAC iPXE boot script)

## 1. Purpose & scope

XBoot coexists with the network's existing router/DHCP. It must **never assign IP
addresses**. It answers only the PXE part of the DHCP exchange so a PXE client learns
*where to get its boot loader*. The router keeps handing out IPs as usual.

This is the classic two-stage PXE/iPXE chainload:

1. Firmware PXE stack does DHCP → we point it at the **iPXE binary** (over TFTP, 06b).
2. iPXE loads, re-does DHCP → we detect iPXE and point it at the **HTTP boot script**
   (06c) instead of the iPXE binary again. This breaks the chainload loop.

### Decisions locked in during brainstorming

| Decision | Choice |
|----------|--------|
| Response mechanism | **`:67` proxy offer + `:4011` PXE Boot Server service** (full PXE spec, max firmware compatibility) |
| Client architectures | **UEFI x64 + legacy BIOS** (option 93: `0x0000` → `undionly.kpxe`; `0x0007`/`0x0009` → `ipxe.efi`) |
| Chainload break | **Built now** — both arms: firmware PXE → TFTP filename; iPXE (option 77 user-class `"iPXE"`) → HTTP script URL |
| Socket level | **UDP datagram + broadcast** (pure-Rust, identical Linux/Windows, testable over loopback). Raw L2 packets deferred. |

### Out of scope (v1)

- IP address assignment (we are a *proxy* DHCP, never authoritative).
- Raw L2 packet crafting / npcap capture (UDP datagram + broadcast is sufficient).
- Interactive PXE boot menus (single boot server, single boot item).
- Architectures beyond UEFI x64 and legacy BIOS.
- Serving the TFTP file (06b) or the HTTP script (06c) — only *pointing* at them.

## 2. Module layout

New code under `crates/xboot-core/src/net/dhcp/`:

| File | Purpose |
|------|---------|
| `dhcp/packet.rs` | DHCP/BOOTP packet **parse + encode**. The fixed BOOTP header (op, htype, hlen, xid, flags, ciaddr/yiaddr/siaddr/giaddr, chaddr) plus the options area as TLV. Pure, no I/O. Parser returns `Result`. |
| `dhcp/options.rs` | Option-code constants and typed accessors: 53 (msg type), 55 (param request list), 60 (vendor class id), 93 (client system arch), 77 (user class), 97 (client machine id / UUID), 54 (server id), 66 (TFTP server name), 67 (bootfile name), 43 (PXE vendor-specific sub-options). |
| `dhcp/decide.rs` | **Pure decision function** `plan(&DhcpMessage, &BootConfig) -> Option<OfferPlan>`. No sockets. Houses the firmware-vs-iPXE and BIOS-vs-UEFI branching — the heart of the phase, fully unit-testable. |
| `dhcp/server.rs` | The async runtime: tokio `UdpSocket` listeners on `:67` and `:4011`; receive → parse → decide → encode → send. |
| `dhcp/mod.rs` | Re-exports; wires the submodule into `net`. |

The existing `net/mod.rs` `NetIo` trait stays as-is. `tokio::net::UdpSocket` already
abstracts Linux vs Windows for the UDP-datagram approach, so no custom socket trait is
needed. Tests run over **real loopback UDP**, mirroring how phase 05c's transport tested
over real loopback TCP. Raw-packet `NetIo` implementations remain deferred.

**Design boundary:** pure decision/codec core (`packet`, `options`, `decide`) is separated
from thin async I/O (`server`). The core is testable without any sockets.

## 3. Data flow

### Server loop (per listener)

1. `recv_from` a datagram → `packet::parse` into a `DhcpMessage`.
2. **Guard:** proceed only if it is a PXE request — message type DISCOVER/REQUEST **and**
   option 60 vendor-class begins with `"PXEClient"`. Everything else is dropped silently,
   so we never interfere with the router's normal DHCP or unrelated UDP traffic.
3. `decide::plan(&msg, &boot_cfg)` → `Option<OfferPlan>` (`None` means "stay silent").
4. `packet::encode(plan)` → send to `255.255.255.255:68` (broadcast), or unicast to
   `giaddr` if the request arrived via a relay agent.

### The decision (`decide::plan`)

```
is the request from iPXE itself?  (option 77 user-class == "iPXE")
├─ yes ─► bootfile = http_script_url + "?mac=<chaddr>"   (HTTP arm — breaks chainload loop)
│          next-server not needed (URL is absolute)
└─ no  ─► look at option 93 (client system architecture):
            0x0000        → bootfile = bios_filename (undionly.kpxe), next-server = server_ip
            0x0007/0x0009 → bootfile = uefi_filename (ipxe.efi),      next-server = server_ip
            unknown arch  → None (log once)
```

### Every offer is a proxy offer

- `yiaddr = 0.0.0.0` — we never assign an IP.
- Echo the client's `xid`, `chaddr`, and broadcast `flags`.
- option 53 = OFFER (or ACK on the `:4011` path).
- option 54 = `server_ip` (server identifier).
- option 60 = `"PXEClient"` so the firmware accepts the offer.
- option 66 (TFTP server) / `siaddr` = `server_ip` on the firmware-PXE arm.
- option 67 = the chosen bootfile or HTTP URL.

### The `:4011` PXE Boot Server service

Compatible firmware, after receiving the proxy offer, sends a PXE Boot Server Request to
`:4011`. That listener replies with a DHCPACK carrying the same boot item + bootfile. Both
listeners share `decide`/`encode`; they differ only in which message types they answer and
in the PXE option-43 sub-options (see §5).

## 4. Configuration

A new optional `[boot]` section in the TOML, parsed and validated in the existing `config`
module:

```toml
[boot]
server_ip       = "192.168.1.10"   # our IP — used for next-server (opt 66) and server-id (opt 54)
bind            = "0.0.0.0"        # listen address for :67 / :4011
bios_filename   = "undionly.kpxe"  # TFTP file for arch 0x0000        (served by 06b)
uefi_filename   = "ipxe.efi"       # TFTP file for arch 0x0007/0x0009 (served by 06b)
http_script_url = "http://192.168.1.10/boot.ipxe"  # iPXE arm; "?mac=..." appended (served by 06c)
```

`BootConfig` struct + a `validate` pass:

- `server_ip` and `bind` parse as `IpAddr`.
- `http_script_url` is non-empty and starts with `http://` or `https://`.
- `bios_filename` and `uefi_filename` are non-empty.

The section is **optional**: if absent, the DHCP server is not started, so existing
configs and iSCSI-only tests continue to work unchanged.

## 5. PXE option 43

For the `:67` + `:4011` flow, the proxy offer carries option 43 (vendor-specific) with the
minimal PXE sub-options:

- `PXE_DISCOVERY_CONTROL` (sub-option 6).
- A single boot-server / boot-item entry directing the client to our `:4011` service.

The `:4011` ACK carries the boot item plus the bootfile. We encode the minimal sub-option
set — **one boot server, one boot item, no interactive menu** ("one image, boot it"). This
broad-compatibility path is the reason `:4011` exists alongside the simpler bootfile-only
offer.

## 6. Error handling

- The parser returns `Result`; malformed/truncated packets are logged once and dropped —
  never panic.
- Non-PXE packets and unknown-arch requests are dropped silently (`decide` returns `None`).
- The server loop survives any single bad datagram and keeps serving.
- No `.unwrap()` / `.expect()` on data derived from the network.

## 7. Testing

Mirrors the project's test stack (master spec §7) and the iSCSI fake-initiator testkit
pattern.

- **Unit** — `packet` parse/encode round-trips, including unknown options and the BOOTP
  fixed header; `options` typed accessors against known byte layouts.
- **Decision tests** — table-driven over `decide::plan`: BIOS arch → `undionly.kpxe`;
  UEFI arch → `ipxe.efi`; iPXE user-class → HTTP URL with MAC appended; non-PXE → `None`;
  unknown arch → `None`.
- **Integration** — a **fake PXE client** over loopback UDP: bind an ephemeral socket,
  send a real DISCOVER, assert the OFFER fields (`yiaddr = 0`, options 53/54/60/66/67,
  correct bootfile). Plus the full two-stage flow: firmware DISCOVER → iPXE re-DISCOVER →
  HTTP URL.
- **proptest** — arbitrary option byte sequences never panic the parser.
- **cargo-fuzz** — a `dhcp_decode` fuzz target, alongside the existing iSCSI PDU target.
