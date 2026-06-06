# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Greenfield. As of this writing the only artifact is the design spec — there is **no Rust code or `Cargo.toml` scaffolded yet**. The authoritative source of truth for what XBoot is and how it should be built is:

> `docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md`

Read that spec before making architectural decisions. Update it (not just code) when scope changes.

## What XBoot is

XBoot is an open-source alternative to **CCboot**: a diskless-boot server. Windows client PCs
PXE-boot over the network from a central golden image — no local disk. Primary scenario: a gaming
club with many identical Windows machines, one master OS image, and a large shared games disk.

Single cross-platform Rust binary (async, tokio). **Production target OS is Windows**; development
runs on Linux. This is why the design is pure-Rust in-process — Linux-kernel-only facilities (LIO,
device-mapper) are deliberately avoided so the same binary works on both.

## Architecture (big picture)

One process, subsystems behind traits so platform-specific code (Windows vs Linux networking) is isolated:

- **Network boot** — `proxyDHCP` (answers only the PXE part of DHCP, never assigns IPs; coexists with
  the existing router/DHCP) → `TFTP` (serves the prebuilt iPXE binary) → `HTTP` (serves a per-MAC iPXE
  boot script). iPXE itself is **not written here** — prebuilt binaries are chainloaded.
- **iSCSI Target** — our own implementation. One target *per client*, multiple LUNs (LUN 0 = system,
  LUN 1.. = games). Implements iSCSI login + a minimal SCSI command set (INQUIRY, REPORT LUNS,
  READ CAPACITY, TEST UNIT READY, READ/WRITE 10/16, SYNCHRONIZE CACHE). Unsupported commands return a
  proper `CHECK CONDITION`, never panic.
- **Volume Manager** — assembles each client's virtual disk from a read-only backing store + a
  per-client copy-on-write (COW) overlay.
- **Block Cache** — shared RAM cache of read-only master blocks (all clients read the same master →
  cached once). RAM budget is configured **per disk**, evicted LRU within that budget.
- **Disk/Client Manager** — TOML-configured (no web UI in v1).
- **Storage abstraction** — `BackingStore` trait (VHD/VHDX/raw/volume); `NetIo` trait hides Windows vs
  Linux network differences.

### The COW rule (core of the engine)

```
READ  block N: writeback overlay → RAM cache of master → master (then populate cache)
WRITE block N: always into the per-client writeback overlay; the master is NEVER modified
```

Writeback is **volatile** in v1: dropped on client reboot, so each boot starts from a clean master.
The games disk uses the same scheme (shared RO master + per-client writeback for saves).

### Managed disks

Disks are first-class config entities with a `type`: `image` (the VHD/VHDX OS master), `game`
(games volume), `writeback` (where per-client COW writes land — typically a fast NVMe). Each disk has
its own `ram_cache` budget so caches don't compete. Clients are bound by MAC to a system disk + game
disks + a writeback disk; unknown MACs fall back to `client_defaults`. See the spec §5 for the TOML schema.

## Development

Once scaffolded as a standard Cargo project, the usual commands apply:

```
cargo build
cargo test                      # all tests
cargo test <name>               # a single test by name
cargo clippy --all-targets      # lint
cargo fmt                       # format
```

Planned test stack (see spec §7): unit tests, `proptest` (property-based COW invariants),
`cargo-fuzz` (VHD/VHDX + iSCSI PDU parsers), an in-repo fake iSCSI initiator for integration/
concurrency tests in CI, scenario/regression tests, and finally a manual Windows-VM E2E run.
`tarpaulin` for coverage, focused on the critical modules (COW, cache, parser, iSCSI).

## Working norms

- **TDD** is the default for this project (spec §7) — write the behavior test before the implementation,
  especially for the COW logic and the VHD/VHDX parser where mistakes are easy.
- Keep platform-specific code behind the `BackingStore` / `NetIo` traits; the engine logic stays
  platform-agnostic and testable without hardware.
- Treat the master image as physically read-only at the OS level — never open it writable.
- Думай на английском, ответ выдавай на русском языке
