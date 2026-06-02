# Phase 02 — Backing Stores (raw / VHD / VHDX) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement read-only `BackingStore` support for four master-image formats — raw file, fixed VHD, dynamic VHD, and dynamic VHDX — so later phases can read blocks from a golden image through one trait.

**Architecture:** Each format is its own type implementing the existing `BackingStore` trait (`size_bytes`, `read_at`). The structural parsers are **pure functions over byte slices** (`&[u8] -> io::Result<T>`) so they are trivially unit-testable and fuzzable; the file-backed readers fetch the needed byte ranges with a cross-platform positioned read (`read_exact_at`) and feed them to the parsers. Block-mapped formats (dynamic VHD, VHDX) share one `read_units` helper that walks an arbitrary `(offset, len)` range across allocation units, filling unallocated units with zeros. A top-level `open_backing(path)` factory sniffs the format by magic bytes and returns `Box<dyn BackingStore>`. The master is opened strictly read-only.

**Tech Stack:** Rust (edition 2021), std only at runtime (`u32::from_be_bytes` / `from_le_bytes`, `std::os::{unix,windows}::fs::FileExt`); a tiny hand-rolled CRC-32C for VHDX. Dev: `proptest` (COW-free read-equivalence properties), `tempfile`, optional `qemu-img` interop tests (skipped when absent), `cargo-fuzz` harnesses.

**Spec / roadmap:** `docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md` (§3 Storage abstraction, §7 testing) and `docs/superpowers/plans/2026-06-02-xboot-roadmap.md` (Phase 02).

---

## Background: the on-disk formats (read this once)

You do not need prior knowledge of VHD/VHDX. Everything required is below; the spec just says "read these formats correctly."

### Raw
The file *is* the disk, byte-for-byte. `size_bytes` = file length, `read_at` = positioned read. No parsing.

### VHD (Microsoft, big-endian fields)
A 512-byte **footer** lives at the **end** of every VHD (and a copy at the start of dynamic VHDs). Footer fields we use (byte offsets within the 512-byte footer):

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0  | 8 | cookie | ASCII `conectix` |
| 16 | 8 | data offset | fixed = `0xFFFFFFFFFFFFFFFF`; dynamic = offset of the dynamic header (usually 512) |
| 48 | 8 | current size | **virtual disk size in bytes** |
| 60 | 4 | disk type | 2 = fixed, 3 = dynamic, 4 = differencing (we reject 4) |
| 64 | 4 | checksum | ones-complement of the sum of all 512 bytes with this field treated as 0 |

- **Fixed VHD:** raw data, then the 512-byte footer. `read_at` reads straight from the file; virtual size = current size.
- **Dynamic VHD:** footer copy (512) → **Dynamic Disk Header** (1024) → **BAT** → data blocks → footer (512). Dynamic header fields (big-endian, offsets within the 1024-byte header):

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0  | 8 | cookie | ASCII `cxsparse` |
| 16 | 8 | table offset | absolute byte offset of the BAT |
| 28 | 4 | max table entries | number of blocks |
| 32 | 4 | block size | bytes per block (power of two, e.g. `0x200000` = 2 MiB) |

The **BAT** is `max_table_entries` big-endian `u32` sector offsets. Entry `0xFFFFFFFF` = block unallocated (reads as zeros). Otherwise the block starts at `entry * 512`; the first bytes are a **sector bitmap** (one bit per 512-byte sector, MSB-first, padded up to a 512-byte boundary), then the block data. A sector whose bitmap bit is 0 reads as zeros.

### VHDX (Microsoft, **little-endian** fields, 1 MiB region alignment)
Layout by fixed file offset:

| Offset | Size | Structure |
|---|---|---|
| 0          | 64 KiB | File Identifier — signature `vhdxfile` |
| 64 KiB     | 4 KiB  | Header 1 |
| 128 KiB    | 4 KiB  | Header 2 |
| 192 KiB    | 64 KiB | Region Table |
| 256 KiB    | 64 KiB | Region Table copy |
| ≥ 1 MiB    | …      | Metadata region + BAT region + payload blocks (located via the region table) |

**Header** (pick the copy with a valid CRC and the greatest sequence number):

| Offset | Size | Field |
|---|---|---|
| 0  | 4  | signature `head` |
| 4  | 4  | CRC-32C over the 4096-byte header with this field zeroed |
| 8  | 8  | sequence number (u64 LE) |
| 48 | 16 | log GUID — **all zero means no log to replay** (we require this) |
| 66 | 2  | version — must be 1 |

**Region Table**: 16-byte header (`regi` signature @0; CRC-32C over the 64 KiB region @4; entry count @8) then 32-byte entries: 16-byte GUID, `u64` file offset, `u32` length, `u32` required-flag. Two GUIDs matter — the **BAT** region and the **Metadata** region.

**Metadata region**: table header (`metadata` signature @0; entry count @10) then 32-byte entries: 16-byte item GUID, `u32` offset (from region start), `u32` length, flags, reserved. Items we read: **File Parameters** (`u32` block size + flags; flag bit1 = HasParent → reject), **Virtual Disk Size** (`u64`), **Logical Sector Size** (`u32`).

**BAT region**: array of `u64` LE entries. Low 3 bits = state, bits 20.. = file offset in **MiB** (byte offset = `entry & !0xFFFFF`). Payload-block (PB) entries are interleaved with sector-bitmap (SB) entries: every `chunk_ratio` PB entries are followed by one SB entry, where `chunk_ratio = (2^23 * logical_sector_size) / block_size`. To find PB entry for block `k`, index `k + k / chunk_ratio`. States: `6` = FULLY_PRESENT (read from offset); `0/1/2/3` = not present / undefined / zero / unmapped (read as zeros); `7` = PARTIALLY_PRESENT (needs the SB block — **rejected in v1**).

GUIDs are stored Microsoft-mixed-endian; we embed them as raw 16-byte arrays and compare bytes, so endianness never bites us.

---

## File structure created in this phase

| File | Responsibility |
|---|---|
| `crates/xboot-core/src/storage/mod.rs` | Trait (exists) + submodule wiring + `open_backing` factory + `invalid_data` helper |
| `crates/xboot-core/src/storage/file_ext.rs` | Cross-platform `read_exact_at(&File, offset, buf)` |
| `crates/xboot-core/src/storage/blockmap.rs` | `read_units` — range walk over allocation units (`Unit::{Zero,At}`) |
| `crates/xboot-core/src/storage/raw.rs` | `RawFile` backing store |
| `crates/xboot-core/src/storage/vhd/mod.rs` | `Vhd` open dispatch (fixed vs dynamic) + module wiring + `#[cfg(test)] fixtures` |
| `crates/xboot-core/src/storage/vhd/footer.rs` | `VhdFooter::parse` + checksum |
| `crates/xboot-core/src/storage/vhd/fixed.rs` | `FixedVhd` backing store |
| `crates/xboot-core/src/storage/vhd/dynamic.rs` | `DynamicVhd` (header + BAT parse + read) |
| `crates/xboot-core/src/storage/vhd/fixtures.rs` | `#[cfg(test)]` builders for footer / fixed / dynamic VHD bytes |
| `crates/xboot-core/src/storage/vhdx/mod.rs` | `Vhdx` backing store + module wiring + `#[cfg(test)] fixtures` |
| `crates/xboot-core/src/storage/vhdx/crc32c.rs` | `crc32c(&[u8]) -> u32` |
| `crates/xboot-core/src/storage/vhdx/structs.rs` | Header + region table parse + region GUIDs |
| `crates/xboot-core/src/storage/vhdx/metadata.rs` | Metadata parse + item GUIDs |
| `crates/xboot-core/src/storage/vhdx/fixtures.rs` | `#[cfg(test)]` builder for a minimal valid VHDX |
| `crates/xboot-core/tests/backing_interop.rs` | Optional `qemu-img` interop tests (skipped if absent) |
| `fuzz/Cargo.toml`, `fuzz/fuzz_targets/*.rs` | cargo-fuzz harnesses for the parsers |

Workspace edits: add `proptest` (dev) and exclude `fuzz/` from the workspace.

---

## Task 1: Dependencies + storage module skeleton

**Files:**
- Modify: `Cargo.toml` (workspace deps + exclude fuzz)
- Modify: `crates/xboot-core/Cargo.toml` (proptest dev-dep)
- Modify: `crates/xboot-core/src/storage/mod.rs`

- [ ] **Step 1: Add `proptest` to workspace deps and exclude the fuzz crate**

In `Cargo.toml`, add `proptest` to `[workspace.dependencies]` and an `exclude` to `[workspace]`:
```toml
[workspace]
resolver = "2"
members = ["crates/xboot-core", "crates/xboot"]
exclude = ["fuzz"]
```
```toml
proptest = "1"
```
(Append the `proptest` line inside the existing `[workspace.dependencies]` table.)

- [ ] **Step 2: Add `proptest` as a dev-dependency of the core crate**

In `crates/xboot-core/Cargo.toml`, under `[dev-dependencies]`:
```toml
proptest.workspace = true
```

- [ ] **Step 3: Extend `storage/mod.rs` with module wiring and shared helpers**

Replace `crates/xboot-core/src/storage/mod.rs` with (the trait body and its test are unchanged from Phase 01; we add submodules, an error helper, and the `open_backing` factory placeholder):
```rust
use std::io;
use std::path::Path;

mod blockmap;
mod file_ext;
mod raw;
mod vhd;
mod vhdx;

pub use raw::RawFile;
pub use vhd::Vhd;
pub use vhdx::Vhdx;

/// A read-only source of disk-image bytes (raw, VHD, VHDX, ...).
pub trait BackingStore: Send + Sync {
    /// Total virtual size in bytes.
    fn size_bytes(&self) -> u64;

    /// Read exactly `buf.len()` bytes starting at byte `offset` into `buf`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// Build an `InvalidData` I/O error from a message. Parsers use this so the
/// `BackingStore` boundary stays `io::Result` without a separate error enum.
pub(crate) fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Open a master image, detecting the format by magic bytes.
/// Implemented in the final task; stubbed so the crate compiles meanwhile.
pub fn open_backing(_path: &Path) -> io::Result<Box<dyn BackingStore>> {
    Err(invalid_data("open_backing not implemented yet"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InMemory(Vec<u8>);

    impl BackingStore for InMemory {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let start = offset as usize;
            buf.copy_from_slice(&self.0[start..start + buf.len()]);
            Ok(())
        }
    }

    #[test]
    fn trait_is_object_safe_and_usable() {
        let store: Box<dyn BackingStore> = Box::new(InMemory(vec![1, 2, 3, 4]));
        let mut buf = [0u8; 2];
        store.read_at(1, &mut buf).unwrap();
        assert_eq!(buf, [2, 3]);
        assert_eq!(store.size_bytes(), 4);
    }
}
```

- [ ] **Step 4: Create empty placeholder module files so the crate compiles**

The `mod` lines above reference files that don't exist yet. Create them with a placeholder comment each (filled in by later tasks):

`crates/xboot-core/src/storage/file_ext.rs`:
```rust
// Filled in Task 2.
```
`crates/xboot-core/src/storage/blockmap.rs`:
```rust
// Filled in Task 3.
```
`crates/xboot-core/src/storage/raw.rs`:
```rust
// Filled in Task 4.
```
`crates/xboot-core/src/storage/vhd/mod.rs`:
```rust
// Filled in Tasks 5-9.
pub struct Vhd;
```
`crates/xboot-core/src/storage/vhdx/mod.rs`:
```rust
// Filled in Tasks 10-14.
pub struct Vhdx;
```

- [ ] **Step 5: Verify it builds**

Run: `cargo build -p xboot-core`
Expected: builds (warnings about unused `Vhd`/`Vhdx`/`RawFile` re-exports are fine for now).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/xboot-core
git commit -m "chore(storage): scaffold backing-store modules and deps"
```

---

## Task 2: Cross-platform positioned read (`read_exact_at`)

**Files:**
- Replace: `crates/xboot-core/src/storage/file_ext.rs`

- [ ] **Step 1: Write the failing test**

Set `crates/xboot-core/src/storage/file_ext.rs` to (test only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_at_offset_without_moving_cursor() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"0123456789").unwrap();
        let file = std::fs::File::open(tmp.path()).unwrap();

        let mut a = [0u8; 3];
        read_exact_at(&file, 2, &mut a).unwrap();
        assert_eq!(&a, b"234");

        // A second positioned read is unaffected by the first (no shared cursor).
        let mut b = [0u8; 2];
        read_exact_at(&file, 0, &mut b).unwrap();
        assert_eq!(&b, b"01");
    }

    #[test]
    fn reading_past_eof_errors() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"abc").unwrap();
        let file = std::fs::File::open(tmp.path()).unwrap();
        let mut buf = [0u8; 8];
        assert!(read_exact_at(&file, 0, &mut buf).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core file_ext::`
Expected: FAIL — `read_exact_at` not found (does not compile).

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/file_ext.rs` (above the `tests` module):
```rust
use std::fs::File;
use std::io;

/// Read exactly `buf.len()` bytes starting at byte `offset`, using a positioned
/// read that does not move (and is unaffected by) the file's cursor — safe to
/// call concurrently through a shared `&File`.
#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

/// Windows equivalent built on `seek_read` (which also does not move the cursor).
#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.seek_read(&mut buf[read..], offset + read as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF in positioned read",
            ));
        }
        read += n;
    }
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core file_ext::`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/file_ext.rs
git commit -m "feat(storage): cross-platform positioned read helper"
```

---

## Task 3: Range walk over allocation units (`read_units`)

This helper powers both block-mapped formats. It splits an arbitrary `(offset, len)` request into per-unit pieces, asks a `locate` closure where each unit lives (or that it's a hole), and either zero-fills or does a positioned read.

**Files:**
- Replace: `crates/xboot-core/src/storage/blockmap.rs`

- [ ] **Step 1: Write the failing test**

Set `crates/xboot-core/src/storage/blockmap.rs` to (test only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    // A fake physical store: unit_size = 4. Unit 0 -> "AAAA", unit 1 -> hole,
    // unit 2 -> "CCCC". Backing bytes live in `phys`.
    fn run(offset: u64, len: usize) -> Vec<u8> {
        let phys: Vec<u8> = b"AAAACCCC".to_vec(); // unit0 at phys 0, unit2 at phys 4
        let mut buf = vec![0u8; len];
        read_units(
            offset,
            &mut buf,
            12, // virtual size: 3 units * 4
            4,
            |unit| {
                Ok(match unit {
                    0 => Unit::At(0),
                    1 => Unit::Zero,
                    2 => Unit::At(4),
                    _ => Unit::Zero,
                })
            },
            |phys_off, dst| {
                let s = phys_off as usize;
                dst.copy_from_slice(&phys[s..s + dst.len()]);
                Ok::<(), io::Error>(())
            },
        )
        .unwrap();
        buf
    }

    #[test]
    fn whole_units() {
        assert_eq!(run(0, 4), b"AAAA");
        assert_eq!(run(4, 4), b"\0\0\0\0"); // the hole
        assert_eq!(run(8, 4), b"CCCC");
    }

    #[test]
    fn spans_units_and_partial_edges() {
        // bytes 2..10 cross unit0 -> unit1(hole) -> unit2
        assert_eq!(run(2, 8), b"AA\0\0\0\0CC");
    }

    #[test]
    fn read_past_end_errors() {
        let mut buf = [0u8; 4];
        let r = read_units(
            10,
            &mut buf,
            12,
            4,
            |_| Ok(Unit::Zero),
            |_, _| Ok::<(), io::Error>(()),
        );
        assert!(r.is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core blockmap::`
Expected: FAIL — `read_units` / `Unit` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/blockmap.rs`:
```rust
use std::io;

use super::invalid_data;

/// Where an allocation unit's data lives.
pub(crate) enum Unit {
    /// Unit is unallocated — reads as zeros.
    Zero,
    /// Unit's data starts at this absolute byte offset in the backing file.
    At(u64),
}

/// Read `buf.len()` bytes starting at virtual byte `offset`, splitting the range
/// across fixed-size allocation units. `locate(unit_index)` says where each unit
/// is; `read_phys(file_offset, dst)` reads `dst.len()` bytes from the file.
///
/// `offset + buf.len()` must not exceed `virtual_size`.
pub(crate) fn read_units(
    offset: u64,
    buf: &mut [u8],
    virtual_size: u64,
    unit_size: u64,
    locate: impl Fn(u64) -> io::Result<Unit>,
    read_phys: impl Fn(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<()> {
    let end = offset
        .checked_add(buf.len() as u64)
        .ok_or_else(|| invalid_data("read range overflows u64"))?;
    if end > virtual_size {
        return Err(invalid_data(format!(
            "read past end of image: {end} > {virtual_size}"
        )));
    }

    let mut pos = offset;
    let mut done = 0usize;
    while done < buf.len() {
        let unit_index = pos / unit_size;
        let within = (pos % unit_size) as usize;
        let take = std::cmp::min(unit_size as usize - within, buf.len() - done);
        let dst = &mut buf[done..done + take];
        match locate(unit_index)? {
            Unit::Zero => dst.fill(0),
            Unit::At(base) => read_phys(base + within as u64, dst)?,
        }
        pos += take as u64;
        done += take;
    }
    Ok(())
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core blockmap::`
Expected: PASS (all three tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/blockmap.rs
git commit -m "feat(storage): range walk over allocation units"
```

---

## Task 4: Raw file backing store

**Files:**
- Replace: `crates/xboot-core/src/storage/raw.rs`

- [ ] **Step 1: Write the failing test**

Set `crates/xboot-core/src/storage/raw.rs` to (test only for now):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::BackingStore;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn size_and_read() {
        let f = write_tmp(b"0123456789");
        let store = RawFile::open(f.path()).unwrap();
        assert_eq!(store.size_bytes(), 10);
        let mut buf = [0u8; 4];
        store.read_at(3, &mut buf).unwrap();
        assert_eq!(&buf, b"3456");
    }

    #[test]
    fn read_past_end_errors() {
        let f = write_tmp(b"abc");
        let store = RawFile::open(f.path()).unwrap();
        let mut buf = [0u8; 4];
        assert!(store.read_at(0, &mut buf).is_err());
    }

    #[test]
    fn missing_file_errors() {
        assert!(RawFile::open(std::path::Path::new("/no/such/raw.img")).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core raw::`
Expected: FAIL — `RawFile` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/raw.rs`:
```rust
use std::fs::File;
use std::io;
use std::path::Path;

use super::file_ext::read_exact_at;
use super::{invalid_data, BackingStore};

/// A backing store over a raw disk image: the file *is* the disk, byte-for-byte.
pub struct RawFile {
    file: File,
    size: u64,
}

impl RawFile {
    /// Open a raw image read-only.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl BackingStore for RawFile {
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| invalid_data("read range overflows u64"))?;
        if end > self.size {
            return Err(invalid_data(format!(
                "read past end of image: {end} > {}",
                self.size
            )));
        }
        read_exact_at(&self.file, offset, buf)
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core raw::`
Expected: PASS (all three tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/raw.rs
git commit -m "feat(storage): raw file backing store"
```

---

## Task 5: VHD test fixtures (footer / fixed / dynamic builders)

These `#[cfg(test)]` builders synthesize valid VHD bytes from a raw blob. Every VHD test (footer, fixed, dynamic, property, factory) uses them — defined once here.

**Files:**
- Modify: `crates/xboot-core/src/storage/vhd/mod.rs` (declare the fixtures module)
- Create: `crates/xboot-core/src/storage/vhd/fixtures.rs`

- [ ] **Step 1: Declare the fixtures module**

Set `crates/xboot-core/src/storage/vhd/mod.rs` to:
```rust
// Backing-store implementations for VHD. Open dispatch added in Task 9.
pub struct Vhd;

#[cfg(test)]
pub(crate) mod fixtures;
```

- [ ] **Step 2: Write the fixtures with a self-check test**

Create `crates/xboot-core/src/storage/vhd/fixtures.rs`:
```rust
//! Builders that synthesize valid VHD byte images for tests.

/// Ones-complement checksum over a 512-byte footer with the checksum field
/// (bytes 64..68) treated as zero.
fn footer_checksum(footer: &[u8; 512]) -> u32 {
    let mut sum: u32 = 0;
    for (i, &b) in footer.iter().enumerate() {
        if (64..68).contains(&i) {
            continue;
        }
        sum = sum.wrapping_add(b as u32);
    }
    !sum
}

/// Build a 512-byte VHD footer. `disk_type` 2 = fixed, 3 = dynamic.
/// `data_offset` is `u64::MAX` for fixed, or the dynamic-header offset.
pub(crate) fn footer(disk_type: u32, current_size: u64, data_offset: u64) -> [u8; 512] {
    let mut f = [0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    f[8..12].copy_from_slice(&0x0000_0002u32.to_be_bytes()); // features
    f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // format version
    f[16..24].copy_from_slice(&data_offset.to_be_bytes());
    f[40..48].copy_from_slice(&current_size.to_be_bytes()); // original size
    f[48..56].copy_from_slice(&current_size.to_be_bytes()); // current size
    f[60..64].copy_from_slice(&disk_type.to_be_bytes());
    let cks = footer_checksum(&f);
    f[64..68].copy_from_slice(&cks.to_be_bytes());
    f
}

/// A fixed VHD: data (already a multiple of 512) followed by the footer.
pub(crate) fn fixed_vhd(data: &[u8]) -> Vec<u8> {
    assert!(data.len() % 512 == 0, "fixed VHD data must be sector-aligned");
    let mut out = data.to_vec();
    out.extend_from_slice(&footer(2, data.len() as u64, u64::MAX));
    out
}

/// A dynamic VHD built from `data` (a multiple of `block_size`). Blocks whose
/// source bytes are all zero are left unallocated (BAT = 0xFFFFFFFF) to exercise
/// the zero path; other blocks are written fully present (bitmap all ones).
pub(crate) fn dynamic_vhd(data: &[u8], block_size: u32) -> Vec<u8> {
    let bs = block_size as usize;
    assert!(bs % 512 == 0 && bs.is_power_of_two());
    assert!(data.len() % bs == 0, "data must be a multiple of block_size");
    let virtual_size = data.len() as u64;
    let n_blocks = (data.len() / bs) as u32;
    let sectors_per_block = block_size / 512;
    let bitmap_bytes_raw = sectors_per_block.div_ceil(8);
    let bitmap_size = bitmap_bytes_raw.div_ceil(512) * 512; // pad to sector

    // Layout: footer copy (512) | dynamic header (1024) | BAT (padded 512) | blocks | footer
    let table_offset: u64 = 512 + 1024;
    let bat_bytes = (n_blocks as u64) * 4;
    let bat_padded = bat_bytes.div_ceil(512) * 512;
    let blocks_start = table_offset + bat_padded;

    let mut out = Vec::new();
    out.extend_from_slice(&footer(3, virtual_size, 512)); // footer copy at start

    // Dynamic disk header (1024 bytes).
    let mut hdr = [0u8; 1024];
    hdr[0..8].copy_from_slice(b"cxsparse");
    hdr[8..16].copy_from_slice(&u64::MAX.to_be_bytes()); // data offset
    hdr[16..24].copy_from_slice(&table_offset.to_be_bytes());
    hdr[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // header version
    hdr[28..32].copy_from_slice(&n_blocks.to_be_bytes()); // max table entries
    hdr[32..36].copy_from_slice(&block_size.to_be_bytes());
    out.extend_from_slice(&hdr);

    // Decide allocation and build the BAT.
    let mut bat = vec![0xFFFF_FFFFu32; n_blocks as usize];
    let mut next_block_sector = (blocks_start / 512) as u32;
    let block_on_disk = bitmap_size + block_size; // bytes per allocated block
    for (i, chunk) in data.chunks(bs).enumerate() {
        if chunk.iter().all(|&b| b == 0) {
            continue; // leave unallocated
        }
        bat[i] = next_block_sector;
        next_block_sector += block_on_disk / 512;
    }
    for entry in &bat {
        out.extend_from_slice(&entry.to_be_bytes());
    }
    // Pad BAT to a sector boundary.
    out.resize(blocks_start as usize, 0);

    // Write allocated blocks: bitmap (all ones for present sectors) + data.
    for (i, chunk) in data.chunks(bs).enumerate() {
        if bat[i] == 0xFFFF_FFFF {
            continue;
        }
        let mut bitmap = vec![0u8; bitmap_size as usize];
        for b in bitmap.iter_mut().take(bitmap_bytes_raw as usize) {
            *b = 0xFF;
        }
        out.extend_from_slice(&bitmap);
        out.extend_from_slice(chunk);
    }

    out.extend_from_slice(&footer(3, virtual_size, 512)); // footer at end
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_has_footer_cookie_at_end() {
        let v = fixed_vhd(&[7u8; 1024]);
        assert_eq!(&v[v.len() - 512..v.len() - 512 + 8], b"conectix");
        assert_eq!(v.len(), 1024 + 512);
    }

    #[test]
    fn dynamic_starts_with_footer_copy_and_header() {
        let v = dynamic_vhd(&[1u8; 8192], 4096);
        assert_eq!(&v[0..8], b"conectix");
        assert_eq!(&v[512..520], b"cxsparse");
    }
}
```

- [ ] **Step 3: Run the self-check tests**

Run: `cargo test -p xboot-core vhd::fixtures::`
Expected: PASS (both tests).

- [ ] **Step 4: Commit**

```bash
git add crates/xboot-core/src/storage/vhd
git commit -m "test(storage): VHD fixture builders"
```

---

## Task 6: VHD footer parser

**Files:**
- Modify: `crates/xboot-core/src/storage/vhd/mod.rs` (declare `footer` module)
- Create: `crates/xboot-core/src/storage/vhd/footer.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Update `crates/xboot-core/src/storage/vhd/mod.rs` to:
```rust
// Backing-store implementations for VHD. Open dispatch added in Task 9.
pub struct Vhd;

mod footer;

#[cfg(test)]
pub(crate) mod fixtures;
```

Create `crates/xboot-core/src/storage/vhd/footer.rs` with the test first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;

    #[test]
    fn parses_fixed_footer() {
        let f = fixtures::footer(2, 4096, u64::MAX);
        let parsed = VhdFooter::parse(&f).unwrap();
        assert_eq!(parsed.disk_type, 2);
        assert_eq!(parsed.current_size, 4096);
        assert_eq!(parsed.data_offset, u64::MAX);
    }

    #[test]
    fn parses_dynamic_footer() {
        let f = fixtures::footer(3, 8192, 512);
        let parsed = VhdFooter::parse(&f).unwrap();
        assert_eq!(parsed.disk_type, 3);
        assert_eq!(parsed.data_offset, 512);
    }

    #[test]
    fn rejects_bad_cookie() {
        let mut f = fixtures::footer(2, 4096, u64::MAX);
        f[0] = b'X';
        assert!(VhdFooter::parse(&f).is_err());
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut f = fixtures::footer(2, 4096, u64::MAX);
        f[100] ^= 0xFF; // corrupt a byte the checksum covers
        assert!(VhdFooter::parse(&f).is_err());
    }

    #[test]
    fn rejects_short_input() {
        assert!(VhdFooter::parse(&[0u8; 100]).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core vhd::footer::`
Expected: FAIL — `VhdFooter` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/vhd/footer.rs`:
```rust
use std::io;

use crate::storage::invalid_data;

/// Disk types we care about.
pub(crate) const DISK_TYPE_FIXED: u32 = 2;
pub(crate) const DISK_TYPE_DYNAMIC: u32 = 3;

/// The parsed fields of a 512-byte VHD footer that we use.
pub(crate) struct VhdFooter {
    pub data_offset: u64,
    pub current_size: u64,
    pub disk_type: u32,
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl VhdFooter {
    /// Parse and validate a footer from at least 512 bytes.
    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 512 {
            return Err(invalid_data("VHD footer shorter than 512 bytes"));
        }
        let f = &bytes[..512];
        if &f[0..8] != b"conectix" {
            return Err(invalid_data("VHD footer cookie mismatch"));
        }

        let stored = be_u32(&f[64..68]);
        let mut sum: u32 = 0;
        for (i, &b) in f.iter().enumerate() {
            if (64..68).contains(&i) {
                continue;
            }
            sum = sum.wrapping_add(b as u32);
        }
        if !sum != stored {
            return Err(invalid_data("VHD footer checksum mismatch"));
        }

        Ok(Self {
            data_offset: be_u64(&f[16..24]),
            current_size: be_u64(&f[48..56]),
            disk_type: be_u32(&f[60..64]),
        })
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core vhd::footer::`
Expected: PASS (all five tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/vhd
git commit -m "feat(storage): parse VHD footer"
```

---

## Task 7: Fixed VHD backing store

**Files:**
- Modify: `crates/xboot-core/src/storage/vhd/mod.rs` (declare `fixed` module)
- Create: `crates/xboot-core/src/storage/vhd/fixed.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Update `crates/xboot-core/src/storage/vhd/mod.rs` to:
```rust
// Backing-store implementations for VHD. Open dispatch added in Task 9.
pub struct Vhd;

mod fixed;
mod footer;

#[cfg(test)]
pub(crate) mod fixtures;
```

Create `crates/xboot-core/src/storage/vhd/fixed.rs` with the test first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;
    use crate::storage::BackingStore;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn reads_data_excluding_footer() {
        let mut data = vec![0u8; 2048];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let img = fixtures::fixed_vhd(&data);
        let tmp = write_tmp(&img);
        let store = FixedVhd::open(tmp.path()).unwrap();

        assert_eq!(store.size_bytes(), 2048); // virtual size, footer excluded
        let mut buf = vec![0u8; 2048];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn read_past_virtual_size_errors() {
        let img = fixtures::fixed_vhd(&[0u8; 512]);
        let tmp = write_tmp(&img);
        let store = FixedVhd::open(tmp.path()).unwrap();
        let mut buf = [0u8; 513];
        assert!(store.read_at(0, &mut buf).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core vhd::fixed::`
Expected: FAIL — `FixedVhd` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/vhd/fixed.rs`:
```rust
use std::fs::File;
use std::io;
use std::path::Path;

use crate::storage::file_ext::read_exact_at;
use crate::storage::vhd::footer::{VhdFooter, DISK_TYPE_FIXED};
use crate::storage::{invalid_data, BackingStore};

/// Fixed VHD: raw data followed by a 512-byte footer. The virtual size is the
/// footer's `current_size`; data starts at byte 0.
pub struct FixedVhd {
    file: File,
    virtual_size: u64,
}

impl FixedVhd {
    /// Open a fixed VHD read-only. Validates the trailing footer.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < 512 {
            return Err(invalid_data("VHD file shorter than a footer"));
        }
        let mut footer_buf = [0u8; 512];
        read_exact_at(&file, file_len - 512, &mut footer_buf)?;
        let footer = VhdFooter::parse(&footer_buf)?;
        if footer.disk_type != DISK_TYPE_FIXED {
            return Err(invalid_data("not a fixed VHD"));
        }
        Ok(Self {
            file,
            virtual_size: footer.current_size,
        })
    }
}

impl BackingStore for FixedVhd {
    fn size_bytes(&self) -> u64 {
        self.virtual_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| invalid_data("read range overflows u64"))?;
        if end > self.virtual_size {
            return Err(invalid_data(format!(
                "read past end of image: {end} > {}",
                self.virtual_size
            )));
        }
        read_exact_at(&self.file, offset, buf)
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core vhd::fixed::`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/vhd
git commit -m "feat(storage): fixed VHD backing store"
```

---

## Task 8: Dynamic VHD backing store (header + BAT + read) + property test

**Files:**
- Modify: `crates/xboot-core/src/storage/vhd/mod.rs` (declare `dynamic` module)
- Create: `crates/xboot-core/src/storage/vhd/dynamic.rs`

- [ ] **Step 1: Declare the module and write the failing unit tests**

Update `crates/xboot-core/src/storage/vhd/mod.rs` to:
```rust
// Backing-store implementations for VHD. Open dispatch added in Task 9.
pub struct Vhd;

mod dynamic;
mod fixed;
mod footer;

#[cfg(test)]
pub(crate) mod fixtures;
```

Create `crates/xboot-core/src/storage/vhd/dynamic.rs` with the tests first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;
    use crate::storage::BackingStore;
    use proptest::prelude::*;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    // Reference blob: 3 blocks of 4096; middle block all zero (left unallocated).
    fn sample_blob() -> Vec<u8> {
        let mut data = vec![0u8; 4096 * 3];
        for i in 0..4096 {
            data[i] = (i % 251) as u8; // block 0
            data[4096 * 2 + i] = (i % 97) as u8 + 1; // block 2 (nonzero)
        }
        data
    }

    #[test]
    fn reads_present_and_unallocated_blocks() {
        let blob = sample_blob();
        let img = fixtures::dynamic_vhd(&blob, 4096);
        let tmp = write_tmp(&img);
        let store = DynamicVhd::open(tmp.path()).unwrap();

        assert_eq!(store.size_bytes(), blob.len() as u64);
        let mut buf = vec![0u8; blob.len()];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, blob);

        // The unallocated middle block reads as zeros.
        let mut mid = vec![0xAAu8; 4096];
        store.read_at(4096, &mut mid).unwrap();
        assert!(mid.iter().all(|&b| b == 0));
    }

    #[test]
    fn read_spanning_block_boundary() {
        let blob = sample_blob();
        let img = fixtures::dynamic_vhd(&blob, 4096);
        let tmp = write_tmp(&img);
        let store = DynamicVhd::open(tmp.path()).unwrap();

        // 100 bytes straddling the block0/block1 boundary.
        let mut buf = vec![0u8; 100];
        store.read_at(4096 - 50, &mut buf).unwrap();
        assert_eq!(&buf[..], &blob[4096 - 50..4096 + 50]);
    }

    proptest! {
        // Reading any sub-range of a dynamic VHD equals the raw reference blob.
        #[test]
        fn reads_match_reference(
            seed in any::<u64>(),
            offset in 0usize..(4096 * 4),
            len in 0usize..512,
        ) {
            // Build a 4-block blob deterministically from the seed; some blocks zero.
            let mut blob = vec![0u8; 4096 * 4];
            let mut x = seed | 1;
            for (i, b) in blob.iter_mut().enumerate() {
                // Zero out whole block 1 and block 3 sometimes to hit holes.
                let block = i / 4096;
                if (block == 1 && seed & 1 == 0) || (block == 3 && seed & 2 == 0) {
                    continue;
                }
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                *b = (x & 0xFF) as u8;
            }
            let img = fixtures::dynamic_vhd(&blob, 4096);
            let tmp = write_tmp(&img);
            let store = DynamicVhd::open(tmp.path()).unwrap();

            let end = (offset + len).min(blob.len());
            let off = offset.min(end);
            let mut buf = vec![0u8; end - off];
            store.read_at(off as u64, &mut buf).unwrap();
            prop_assert_eq!(&buf[..], &blob[off..end]);
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p xboot-core vhd::dynamic::`
Expected: FAIL — `DynamicVhd` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/vhd/dynamic.rs`:
```rust
use std::fs::File;
use std::io;
use std::path::Path;

use crate::storage::blockmap::{read_units, Unit};
use crate::storage::file_ext::read_exact_at;
use crate::storage::vhd::footer::{VhdFooter, DISK_TYPE_DYNAMIC};
use crate::storage::{invalid_data, BackingStore};

const SECTOR: u64 = 512;

/// Dynamic VHD: footer + dynamic header + BAT + sparse blocks.
pub struct DynamicVhd {
    file: File,
    virtual_size: u64,
    block_size: u64,
    bitmap_size: u64,
    /// One BAT entry per block: sector offset of the block, or `0xFFFFFFFF`.
    bat: Vec<u32>,
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl DynamicVhd {
    /// Open a dynamic VHD read-only. Parses the footer, dynamic header, and BAT.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < 512 {
            return Err(invalid_data("VHD file shorter than a footer"));
        }

        let mut footer_buf = [0u8; 512];
        read_exact_at(&file, file_len - 512, &mut footer_buf)?;
        let footer = VhdFooter::parse(&footer_buf)?;
        if footer.disk_type != DISK_TYPE_DYNAMIC {
            return Err(invalid_data("not a dynamic VHD"));
        }
        let virtual_size = footer.current_size;

        // Dynamic disk header at footer.data_offset.
        let mut hdr = [0u8; 1024];
        read_exact_at(&file, footer.data_offset, &mut hdr)?;
        if &hdr[0..8] != b"cxsparse" {
            return Err(invalid_data("dynamic header cookie mismatch"));
        }
        let table_offset = be_u64(&hdr[16..24]);
        let max_entries = be_u32(&hdr[28..32]) as u64;
        let block_size = be_u32(&hdr[32..36]) as u64;
        if block_size == 0 || block_size % SECTOR != 0 || !block_size.is_power_of_two() {
            return Err(invalid_data("invalid dynamic VHD block size"));
        }
        // Guard against absurd BAT sizes from corrupt headers.
        if max_entries > (1u64 << 32) {
            return Err(invalid_data("dynamic VHD BAT too large"));
        }

        let sectors_per_block = block_size / SECTOR;
        let bitmap_bytes_raw = sectors_per_block.div_ceil(8);
        let bitmap_size = bitmap_bytes_raw.div_ceil(SECTOR) * SECTOR;

        // Read the BAT.
        let bat_bytes = (max_entries as usize)
            .checked_mul(4)
            .ok_or_else(|| invalid_data("BAT size overflow"))?;
        let mut raw = vec![0u8; bat_bytes];
        read_exact_at(&file, table_offset, &mut raw)?;
        let bat: Vec<u32> = raw.chunks_exact(4).map(be_u32).collect();

        Ok(Self {
            file,
            virtual_size,
            block_size,
            bitmap_size,
            bat,
        })
    }
}

impl BackingStore for DynamicVhd {
    fn size_bytes(&self) -> u64 {
        self.virtual_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        // Allocation unit = one 512-byte sector (honors the per-sector bitmap).
        read_units(
            offset,
            buf,
            self.virtual_size,
            SECTOR,
            |sector| {
                let block = (sector / (self.block_size / SECTOR)) as usize;
                let entry = *self
                    .bat
                    .get(block)
                    .ok_or_else(|| invalid_data("sector beyond BAT"))?;
                if entry == 0xFFFF_FFFF {
                    return Ok(Unit::Zero);
                }
                let block_start = entry as u64 * SECTOR;
                let sector_in_block = sector % (self.block_size / SECTOR);

                // Consult the per-sector bitmap (MSB-first).
                let byte_idx = sector_in_block / 8;
                let bit = 7 - (sector_in_block % 8);
                let mut one = [0u8; 1];
                read_exact_at(&self.file, block_start + byte_idx, &mut one)?;
                if one[0] & (1 << bit) == 0 {
                    return Ok(Unit::Zero);
                }
                let data_off = block_start + self.bitmap_size + sector_in_block * SECTOR;
                Ok(Unit::At(data_off))
            },
            |phys_off, dst| read_exact_at(&self.file, phys_off, dst),
        )
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p xboot-core vhd::dynamic::`
Expected: PASS (unit tests + the `reads_match_reference` property test).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/vhd
git commit -m "feat(storage): dynamic VHD backing store with COW-free read property test"
```

---

## Task 9: VHD open dispatch (`Vhd::open`)

A single entry point that reads the footer once and returns the right concrete reader as a boxed `BackingStore`.

**Files:**
- Replace: `crates/xboot-core/src/storage/vhd/mod.rs`

- [ ] **Step 1: Write the failing test (and convert `Vhd` from a unit struct to a real type)**

Replace `crates/xboot-core/src/storage/vhd/mod.rs` with:
```rust
//! VHD backing stores. `Vhd::open` sniffs the footer's disk type and returns
//! the matching reader.

use std::io;
use std::path::Path;

mod dynamic;
mod fixed;
mod footer;

pub use dynamic::DynamicVhd;
pub use fixed::FixedVhd;

use crate::storage::file_ext::read_exact_at;
use crate::storage::{invalid_data, BackingStore};
use footer::{VhdFooter, DISK_TYPE_DYNAMIC, DISK_TYPE_FIXED};

/// Open a VHD (fixed or dynamic) read-only as a boxed backing store.
pub struct Vhd;

impl Vhd {
    pub fn open(path: &Path) -> io::Result<Box<dyn BackingStore>> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len < 512 {
            return Err(invalid_data("VHD file shorter than a footer"));
        }
        let mut footer_buf = [0u8; 512];
        read_exact_at(&file, len - 512, &mut footer_buf)?;
        let footer = VhdFooter::parse(&footer_buf)?;
        match footer.disk_type {
            DISK_TYPE_FIXED => Ok(Box::new(FixedVhd::open(path)?)),
            DISK_TYPE_DYNAMIC => Ok(Box::new(DynamicVhd::open(path)?)),
            other => Err(invalid_data(format!("unsupported VHD disk type {other}"))),
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhd::fixtures;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn dispatches_fixed() {
        let tmp = write_tmp(&fixtures::fixed_vhd(&[5u8; 1024]));
        let store = Vhd::open(tmp.path()).unwrap();
        assert_eq!(store.size_bytes(), 1024);
    }

    #[test]
    fn dispatches_dynamic() {
        let tmp = write_tmp(&fixtures::dynamic_vhd(&[6u8; 8192], 4096));
        let store = Vhd::open(tmp.path()).unwrap();
        assert_eq!(store.size_bytes(), 8192);
    }

    #[test]
    fn rejects_differencing() {
        // disk_type 4 = differencing, unsupported.
        let mut img = fixtures::fixed_vhd(&[0u8; 512]);
        let foot = fixtures::footer(4, 512, u64::MAX);
        let n = img.len();
        img[n - 512..].copy_from_slice(&foot);
        let tmp = write_tmp(&img);
        assert!(Vhd::open(tmp.path()).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p xboot-core vhd::tests::`
Expected: PASS (all three). Also rerun `cargo test -p xboot-core vhd::` to confirm the whole VHD module is green.

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/storage/vhd
git commit -m "feat(storage): VHD open dispatch (fixed/dynamic)"
```

---

## Task 10: CRC-32C (Castagnoli)

VHDX validates its headers and region table with CRC-32C. A tiny bitwise implementation is enough.

**Files:**
- Modify: `crates/xboot-core/src/storage/vhdx/mod.rs` (declare `crc32c` module)
- Create: `crates/xboot-core/src/storage/vhdx/crc32c.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Set `crates/xboot-core/src/storage/vhdx/mod.rs` to:
```rust
// VHDX backing store. Filled in Tasks 11-14.
pub struct Vhdx;

mod crc32c;
```

Create `crates/xboot-core/src/storage/vhdx/crc32c.rs` with the test first:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Known CRC-32C test vectors (Castagnoli, init 0xFFFFFFFF, final XOR).
    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core vhdx::crc32c::`
Expected: FAIL — `crc32c` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/vhdx/crc32c.rs`:
```rust
/// CRC-32C (Castagnoli, polynomial 0x82F63B78 reflected), init 0xFFFFFFFF,
/// final XOR 0xFFFFFFFF — the variant VHDX uses.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    !crc
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core vhdx::crc32c::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/vhdx
git commit -m "feat(storage): CRC-32C for VHDX validation"
```

---

## Task 11: VHDX test fixture builder

A `#[cfg(test)]` builder that synthesizes a minimal, valid dynamic VHDX (1 MiB blocks, single header/region table/metadata/BAT, empty log) from a raw blob. Used by every VHDX test.

**Files:**
- Modify: `crates/xboot-core/src/storage/vhdx/mod.rs` (declare `fixtures` module)
- Create: `crates/xboot-core/src/storage/vhdx/fixtures.rs`

- [ ] **Step 1: Declare the fixtures module**

Update `crates/xboot-core/src/storage/vhdx/mod.rs` to:
```rust
// VHDX backing store. Filled in Tasks 12-14.
pub struct Vhdx;

mod crc32c;

#[cfg(test)]
pub(crate) mod fixtures;
```

- [ ] **Step 2: Write the fixture builder with a self-check test**

Create `crates/xboot-core/src/storage/vhdx/fixtures.rs`:
```rust
//! Build a minimal valid dynamic VHDX from a raw blob, for tests.
//!
//! Layout (1 MiB region alignment, 1 MiB payload blocks):
//!   0       File Identifier ("vhdxfile")
//!   64 KiB  Header 1 (sequence 2, log GUID = 0)
//!   128 KiB Header 2 (sequence 1)
//!   192 KiB Region Table (BAT + Metadata entries)
//!   256 KiB Region Table copy
//!   1 MiB   Metadata region (1 MiB)
//!   2 MiB   BAT region (1 MiB)
//!   3 MiB.. payload blocks, one per MiB

use super::crc32c::crc32c;

const KIB: usize = 1024;
const MIB: u64 = 1024 * 1024;
pub(crate) const BLOCK_SIZE: u64 = MIB;
const LOGICAL_SECTOR: u32 = 512;

// Region GUIDs (raw 16-byte, Microsoft mixed-endian).
const BAT_GUID: [u8; 16] = [
    0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08,
];
const METADATA_GUID: [u8; 16] = [
    0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E,
];
// Metadata item GUIDs.
const FILE_PARAMS_GUID: [u8; 16] = [
    0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B,
];
const VDISK_SIZE_GUID: [u8; 16] = [
    0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8,
];
const LOGICAL_SECTOR_GUID: [u8; 16] = [
    0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F,
];

const METADATA_REGION_OFF: u64 = MIB;
const BAT_REGION_OFF: u64 = 2 * MIB;
const FIRST_BLOCK_OFF: u64 = 3 * MIB;

fn write_header(buf: &mut [u8], sequence: u64) {
    // 4 KiB header at buf[..4096].
    let h = &mut buf[..4096];
    h[0..4].copy_from_slice(b"head");
    h[8..16].copy_from_slice(&sequence.to_le_bytes());
    // log guid (48..64) stays zero -> no log to replay.
    h[66..68].copy_from_slice(&1u16.to_le_bytes()); // version
    let crc = crc32c(h); // checksum field (4..8) currently zero
    h[4..8].copy_from_slice(&crc.to_le_bytes());
}

fn write_region_table(buf: &mut [u8], bat_len: u64) {
    // 64 KiB region table at buf[..65536].
    let r = &mut buf[..64 * KIB];
    r[0..4].copy_from_slice(b"regi");
    r[8..12].copy_from_slice(&2u32.to_le_bytes()); // entry count
    // Entry 0: BAT.
    let e0 = 16;
    r[e0..e0 + 16].copy_from_slice(&BAT_GUID);
    r[e0 + 16..e0 + 24].copy_from_slice(&BAT_REGION_OFF.to_le_bytes());
    r[e0 + 24..e0 + 28].copy_from_slice(&(bat_len as u32).to_le_bytes());
    r[e0 + 28..e0 + 32].copy_from_slice(&1u32.to_le_bytes()); // required
    // Entry 1: Metadata.
    let e1 = 16 + 32;
    r[e1..e1 + 16].copy_from_slice(&METADATA_GUID);
    r[e1 + 16..e1 + 24].copy_from_slice(&METADATA_REGION_OFF.to_le_bytes());
    r[e1 + 24..e1 + 28].copy_from_slice(&(MIB as u32).to_le_bytes());
    r[e1 + 28..e1 + 32].copy_from_slice(&1u32.to_le_bytes());
    let crc = crc32c(r);
    r[4..8].copy_from_slice(&crc.to_le_bytes());
}

fn write_metadata(buf: &mut [u8], virtual_size: u64) {
    // Metadata region (1 MiB) at buf[..MIB].
    let m = &mut buf[..MIB as usize];
    m[0..8].copy_from_slice(b"metadata");
    m[10..12].copy_from_slice(&3u16.to_le_bytes()); // entry count

    // Item values live at fixed offsets within the region.
    let fp_off: u32 = 0x1_0000; // 64 KiB
    let vs_off: u32 = 0x1_0010;
    let ls_off: u32 = 0x1_0020;

    let mut put_entry = |slot: usize, guid: &[u8; 16], off: u32, len: u32| {
        let e = 32 + slot * 32; // entries start at offset 32
        m[e..e + 16].copy_from_slice(guid);
        m[e + 16..e + 20].copy_from_slice(&off.to_le_bytes());
        m[e + 20..e + 24].copy_from_slice(&len.to_le_bytes());
        m[e + 24..e + 28].copy_from_slice(&0u32.to_le_bytes()); // flags
    };
    put_entry(0, &FILE_PARAMS_GUID, fp_off, 8);
    put_entry(1, &VDISK_SIZE_GUID, vs_off, 8);
    put_entry(2, &LOGICAL_SECTOR_GUID, ls_off, 4);

    // File Parameters: block size (4) + flags (4) [bit1 HasParent = 0].
    m[fp_off as usize..fp_off as usize + 4].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    m[fp_off as usize + 4..fp_off as usize + 8].copy_from_slice(&0u32.to_le_bytes());
    // Virtual disk size (8).
    m[vs_off as usize..vs_off as usize + 8].copy_from_slice(&virtual_size.to_le_bytes());
    // Logical sector size (4).
    m[ls_off as usize..ls_off as usize + 4].copy_from_slice(&LOGICAL_SECTOR.to_le_bytes());
}

/// Build a dynamic VHDX whose virtual disk equals `data` (a multiple of 1 MiB).
/// All-zero blocks are left not-present (BAT state 0) to exercise the zero path.
pub(crate) fn dynamic_vhdx(data: &[u8]) -> Vec<u8> {
    assert!(data.len() as u64 % BLOCK_SIZE == 0, "data must be MiB-aligned");
    let virtual_size = data.len() as u64;
    let n_blocks = virtual_size / BLOCK_SIZE;

    // chunk_ratio for 512-byte sectors and 1 MiB blocks = 4096; with few blocks
    // every PB index < chunk_ratio, so no SB entries interleave.
    let bat_entries = n_blocks; // PB entries only (n_blocks < 4096 in tests)
    let bat_len = MIB; // we reserve a full 1 MiB region

    // Assemble the file up to the first payload block (3 MiB), then append blocks.
    let mut out = vec![0u8; FIRST_BLOCK_OFF as usize];

    out[0..8].copy_from_slice(b"vhdxfile"); // File Identifier

    write_header(&mut out[64 * KIB..], 2); // Header 1, higher sequence
    write_header(&mut out[128 * KIB..], 1); // Header 2
    write_region_table(&mut out[192 * KIB..], bat_len);
    write_region_table(&mut out[256 * KIB..], bat_len);
    write_metadata(&mut out[METADATA_REGION_OFF as usize..], virtual_size);

    // Build BAT and append present blocks at 3 MiB, 4 MiB, ...
    let mut next_off = FIRST_BLOCK_OFF;
    let mut bat = vec![0u64; bat_entries as usize];
    for (i, chunk) in data.chunks(BLOCK_SIZE as usize).enumerate() {
        if chunk.iter().all(|&b| b == 0) {
            continue; // state 0 = not present -> zeros
        }
        bat[i] = (next_off & !0xFFFFF) | 6; // FULLY_PRESENT
        out.extend_from_slice(chunk);
        next_off += BLOCK_SIZE;
    }
    // Write the BAT entries into the reserved BAT region.
    let bat_start = BAT_REGION_OFF as usize;
    for (i, e) in bat.iter().enumerate() {
        out[bat_start + i * 8..bat_start + i * 8 + 8].copy_from_slice(&e.to_le_bytes());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_identifier_and_header_signatures() {
        let v = dynamic_vhdx(&[1u8; BLOCK_SIZE as usize]);
        assert_eq!(&v[0..8], b"vhdxfile");
        assert_eq!(&v[64 * KIB..64 * KIB + 4], b"head");
        assert_eq!(&v[192 * KIB..192 * KIB + 4], b"regi");
    }
}
```

- [ ] **Step 3: Run the self-check test**

Run: `cargo test -p xboot-core vhdx::fixtures::`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/xboot-core/src/storage/vhdx
git commit -m "test(storage): minimal VHDX fixture builder"
```

---

## Task 12: VHDX header + region table parse

**Files:**
- Modify: `crates/xboot-core/src/storage/vhdx/mod.rs` (declare `structs` module)
- Create: `crates/xboot-core/src/storage/vhdx/structs.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Update `crates/xboot-core/src/storage/vhdx/mod.rs` to:
```rust
// VHDX backing store. Read path added in Tasks 13-14.
pub struct Vhdx;

mod crc32c;
mod structs;

#[cfg(test)]
pub(crate) mod fixtures;
```

Create `crates/xboot-core/src/storage/vhdx/structs.rs` with the test first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhdx::fixtures;

    #[test]
    fn picks_header_with_higher_sequence() {
        let img = fixtures::dynamic_vhdx(&[1u8; 1024 * 1024]);
        let h1 = &img[64 * 1024..64 * 1024 + 4096];
        let h2 = &img[128 * 1024..128 * 1024 + 4096];
        let chosen = parse_best_header(h1, h2).unwrap();
        assert_eq!(chosen.sequence, 2);
    }

    #[test]
    fn rejects_header_with_bad_crc() {
        let img = fixtures::dynamic_vhdx(&[1u8; 1024 * 1024]);
        let mut h = img[64 * 1024..64 * 1024 + 4096].to_vec();
        h[100] ^= 0xFF;
        // Both copies corrupted -> no valid header.
        assert!(parse_best_header(&h, &h).is_err());
    }

    #[test]
    fn finds_bat_and_metadata_regions() {
        let img = fixtures::dynamic_vhdx(&[1u8; 1024 * 1024]);
        let rt = &img[192 * 1024..192 * 1024 + 64 * 1024];
        let regions = parse_region_table(rt).unwrap();
        assert!(regions.bat.is_some());
        assert!(regions.metadata.is_some());
        let (off, _len) = regions.metadata.unwrap();
        assert_eq!(off, 1024 * 1024);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core vhdx::structs::`
Expected: FAIL — `parse_best_header` / `parse_region_table` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/vhdx/structs.rs`:
```rust
use std::io;

use crate::storage::invalid_data;

use super::crc32c::crc32c;

pub(crate) const BAT_GUID: [u8; 16] = [
    0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08,
];
pub(crate) const METADATA_GUID: [u8; 16] = [
    0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B, 0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E,
];

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

pub(crate) struct VhdxHeader {
    pub sequence: u64,
}

/// Parse one 4096-byte header if its signature, version, CRC, and empty-log
/// invariant hold.
fn parse_one_header(h: &[u8]) -> io::Result<VhdxHeader> {
    if h.len() < 4096 || &h[0..4] != b"head" {
        return Err(invalid_data("VHDX header signature mismatch"));
    }
    let stored = le_u32(&h[4..8]);
    let mut tmp = h[..4096].to_vec();
    tmp[4..8].fill(0);
    if crc32c(&tmp) != stored {
        return Err(invalid_data("VHDX header CRC mismatch"));
    }
    let version = u16::from_le_bytes([h[66], h[67]]);
    if version != 1 {
        return Err(invalid_data("unsupported VHDX header version"));
    }
    // Log GUID must be all-zero (no log to replay).
    if h[48..64].iter().any(|&b| b != 0) {
        return Err(invalid_data("VHDX has a non-empty log (dirty image)"));
    }
    Ok(VhdxHeader {
        sequence: le_u64(&h[8..16]),
    })
}

/// Choose the valid header with the greatest sequence number.
pub(crate) fn parse_best_header(h1: &[u8], h2: &[u8]) -> io::Result<VhdxHeader> {
    let a = parse_one_header(h1).ok();
    let b = parse_one_header(h2).ok();
    match (a, b) {
        (Some(a), Some(b)) => Ok(if a.sequence >= b.sequence { a } else { b }),
        (Some(a), None) => Ok(a),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(invalid_data("no valid VHDX header")),
    }
}

/// Located regions: each is `(file_offset, length)`.
pub(crate) struct Regions {
    pub bat: Option<(u64, u64)>,
    pub metadata: Option<(u64, u64)>,
}

/// Parse a 64 KiB region table, validating its CRC, and pick out the BAT and
/// metadata regions by GUID.
pub(crate) fn parse_region_table(rt: &[u8]) -> io::Result<Regions> {
    if rt.len() < 64 * 1024 || &rt[0..4] != b"regi" {
        return Err(invalid_data("VHDX region table signature mismatch"));
    }
    let stored = le_u32(&rt[4..8]);
    let mut tmp = rt[..64 * 1024].to_vec();
    tmp[4..8].fill(0);
    if crc32c(&tmp) != stored {
        return Err(invalid_data("VHDX region table CRC mismatch"));
    }
    let count = le_u32(&rt[8..12]) as usize;
    // Each entry is 32 bytes; entries start at offset 16. Bound the count.
    if 16 + count * 32 > 64 * 1024 {
        return Err(invalid_data("VHDX region table entry count too large"));
    }

    let mut regions = Regions {
        bat: None,
        metadata: None,
    };
    for i in 0..count {
        let e = 16 + i * 32;
        let guid = &rt[e..e + 16];
        let off = le_u64(&rt[e + 16..e + 24]);
        let len = le_u32(&rt[e + 24..e + 28]) as u64;
        if guid == BAT_GUID {
            regions.bat = Some((off, len));
        } else if guid == METADATA_GUID {
            regions.metadata = Some((off, len));
        }
    }
    Ok(regions)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core vhdx::structs::`
Expected: PASS (all three tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/vhdx
git commit -m "feat(storage): parse VHDX header and region table"
```

---

## Task 13: VHDX metadata parse

**Files:**
- Modify: `crates/xboot-core/src/storage/vhdx/mod.rs` (declare `metadata` module)
- Create: `crates/xboot-core/src/storage/vhdx/metadata.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Update `crates/xboot-core/src/storage/vhdx/mod.rs` to:
```rust
// VHDX backing store. Read path added in Task 14.
pub struct Vhdx;

mod crc32c;
mod metadata;
mod structs;

#[cfg(test)]
pub(crate) mod fixtures;
```

Create `crates/xboot-core/src/storage/vhdx/metadata.rs` with the test first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhdx::fixtures;

    #[test]
    fn reads_block_size_sector_and_virtual_size() {
        let img = fixtures::dynamic_vhdx(&[1u8; 2 * 1024 * 1024]);
        let region = &img[1024 * 1024..2 * 1024 * 1024]; // metadata region (1 MiB)
        let md = parse_metadata(region).unwrap();
        assert_eq!(md.block_size, 1024 * 1024);
        assert_eq!(md.logical_sector_size, 512);
        assert_eq!(md.virtual_size, 2 * 1024 * 1024);
    }

    #[test]
    fn rejects_bad_signature() {
        let mut region = vec![0u8; 1024 * 1024];
        region[0..8].copy_from_slice(b"NOTMETA!");
        assert!(parse_metadata(&region).is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core vhdx::metadata::`
Expected: FAIL — `parse_metadata` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/storage/vhdx/metadata.rs`:
```rust
use std::io;

use crate::storage::invalid_data;

const FILE_PARAMS_GUID: [u8; 16] = [
    0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B,
];
const VDISK_SIZE_GUID: [u8; 16] = [
    0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8,
];
const LOGICAL_SECTOR_GUID: [u8; 16] = [
    0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F,
];

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

pub(crate) struct VhdxMetadata {
    pub block_size: u64,
    pub logical_sector_size: u64,
    pub virtual_size: u64,
}

/// Parse the metadata region: its table header plus the three items we need.
/// Rejects images with a parent (differencing VHDX) — masters are standalone.
pub(crate) fn parse_metadata(region: &[u8]) -> io::Result<VhdxMetadata> {
    if region.len() < 32 || &region[0..8] != b"metadata" {
        return Err(invalid_data("VHDX metadata signature mismatch"));
    }
    let count = le_u16(&region[10..12]) as usize;
    if 32 + count * 32 > region.len() {
        return Err(invalid_data("VHDX metadata entry count too large"));
    }

    let mut block_size: Option<u64> = None;
    let mut logical_sector_size: Option<u64> = None;
    let mut virtual_size: Option<u64> = None;

    for i in 0..count {
        let e = 32 + i * 32;
        let guid = &region[e..e + 16];
        let off = le_u32(&region[e + 16..e + 20]) as usize;
        let len = le_u32(&region[e + 20..e + 24]) as usize;
        let end = off
            .checked_add(len)
            .filter(|&end| end <= region.len())
            .ok_or_else(|| invalid_data("VHDX metadata item out of bounds"))?;
        let value = &region[off..end];

        if guid == FILE_PARAMS_GUID {
            if value.len() < 8 {
                return Err(invalid_data("VHDX file parameters too short"));
            }
            block_size = Some(le_u32(&value[0..4]) as u64);
            let flags = le_u32(&value[4..8]);
            if flags & 0b10 != 0 {
                return Err(invalid_data("differencing VHDX (has parent) unsupported"));
            }
        } else if guid == VDISK_SIZE_GUID {
            if value.len() < 8 {
                return Err(invalid_data("VHDX virtual disk size too short"));
            }
            virtual_size = Some(le_u64(&value[0..8]));
        } else if guid == LOGICAL_SECTOR_GUID {
            if value.len() < 4 {
                return Err(invalid_data("VHDX logical sector size too short"));
            }
            logical_sector_size = Some(le_u32(&value[0..4]) as u64);
        }
    }

    let block_size = block_size.ok_or_else(|| invalid_data("VHDX missing block size"))?;
    let logical_sector_size =
        logical_sector_size.ok_or_else(|| invalid_data("VHDX missing logical sector size"))?;
    let virtual_size = virtual_size.ok_or_else(|| invalid_data("VHDX missing virtual size"))?;

    if block_size == 0 || !block_size.is_power_of_two() {
        return Err(invalid_data("invalid VHDX block size"));
    }
    if logical_sector_size == 0 || !logical_sector_size.is_power_of_two() {
        return Err(invalid_data("invalid VHDX logical sector size"));
    }
    Ok(VhdxMetadata {
        block_size,
        logical_sector_size,
        virtual_size,
    })
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core vhdx::metadata::`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/vhdx
git commit -m "feat(storage): parse VHDX metadata"
```

---

## Task 14: VHDX backing store (BAT read + `Vhdx::open`) + property test

**Files:**
- Replace: `crates/xboot-core/src/storage/vhdx/mod.rs`

- [ ] **Step 1: Write the failing tests (and turn `Vhdx` into a real backing store)**

Replace `crates/xboot-core/src/storage/vhdx/mod.rs` with:
```rust
//! Dynamic VHDX backing store (read-only, clean image / empty log).

use std::fs::File;
use std::io;
use std::path::Path;

mod crc32c;
mod metadata;
mod structs;

use crate::storage::blockmap::{read_units, Unit};
use crate::storage::file_ext::read_exact_at;
use crate::storage::{invalid_data, BackingStore};
use metadata::parse_metadata;
use structs::{parse_best_header, parse_region_table};

const PB_STATE_FULLY_PRESENT: u64 = 6;
const PB_STATE_PARTIALLY_PRESENT: u64 = 7;

pub struct Vhdx {
    file: File,
    virtual_size: u64,
    block_size: u64,
    chunk_ratio: u64,
    /// Raw BAT entries (payload + interleaved sector-bitmap entries).
    bat: Vec<u64>,
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl Vhdx {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;

        // File identifier.
        let mut ident = [0u8; 8];
        read_exact_at(&file, 0, &mut ident)?;
        if &ident != b"vhdxfile" {
            return Err(invalid_data("not a VHDX file"));
        }

        // Headers at 64 KiB and 128 KiB.
        let mut h1 = vec![0u8; 4096];
        let mut h2 = vec![0u8; 4096];
        read_exact_at(&file, 64 * 1024, &mut h1)?;
        read_exact_at(&file, 128 * 1024, &mut h2)?;
        let _header = parse_best_header(&h1, &h2)?;

        // Region table at 192 KiB (copy at 256 KiB).
        let mut rt = vec![0u8; 64 * 1024];
        if read_exact_at(&file, 192 * 1024, &mut rt)
            .and_then(|_| parse_region_table(&rt))
            .is_err()
        {
            read_exact_at(&file, 256 * 1024, &mut rt)?;
        }
        let regions = parse_region_table(&rt)?;
        let (meta_off, meta_len) = regions
            .metadata
            .ok_or_else(|| invalid_data("VHDX missing metadata region"))?;
        let (bat_off, bat_len) = regions
            .bat
            .ok_or_else(|| invalid_data("VHDX missing BAT region"))?;

        // Metadata.
        let mut meta = vec![0u8; meta_len as usize];
        read_exact_at(&file, meta_off, &mut meta)?;
        let md = parse_metadata(&meta)?;

        // chunk_ratio = (2^23 * logical_sector_size) / block_size.
        let chunk_ratio = (8_388_608u64 * md.logical_sector_size) / md.block_size;
        if chunk_ratio == 0 {
            return Err(invalid_data("invalid VHDX chunk ratio"));
        }

        // BAT.
        let mut raw = vec![0u8; bat_len as usize];
        read_exact_at(&file, bat_off, &mut raw)?;
        let bat: Vec<u64> = raw.chunks_exact(8).map(le_u64).collect();

        Ok(Self {
            file,
            virtual_size: md.virtual_size,
            block_size: md.block_size,
            chunk_ratio,
            bat,
        })
    }
}

impl BackingStore for Vhdx {
    fn size_bytes(&self) -> u64 {
        self.virtual_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_units(
            offset,
            buf,
            self.virtual_size,
            self.block_size,
            |block| {
                // PB entry index skips one SB entry per chunk_ratio payload blocks.
                let idx = (block + block / self.chunk_ratio) as usize;
                let entry = *self
                    .bat
                    .get(idx)
                    .ok_or_else(|| invalid_data("block beyond BAT"))?;
                let state = entry & 0x7;
                match state {
                    PB_STATE_FULLY_PRESENT => Ok(Unit::At(entry & !0xFFFFF)),
                    PB_STATE_PARTIALLY_PRESENT => {
                        Err(invalid_data("partially-present VHDX block unsupported"))
                    }
                    _ => Ok(Unit::Zero), // not present / undefined / zero / unmapped
                }
            },
            |phys_off, dst| read_exact_at(&self.file, phys_off, dst),
        )
    }
}

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vhdx::fixtures;
    use proptest::prelude::*;

    fn write_tmp(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    fn sample_blob() -> Vec<u8> {
        let mb = 1024 * 1024;
        let mut data = vec![0u8; mb * 3];
        for i in 0..mb {
            data[i] = (i % 251) as u8; // block 0 present
            data[mb * 2 + i] = ((i % 97) + 1) as u8; // block 2 present
        } // block 1 left zero -> not present
        data
    }

    #[test]
    fn reads_present_and_zero_blocks() {
        let blob = sample_blob();
        let tmp = write_tmp(&fixtures::dynamic_vhdx(&blob));
        let store = Vhdx::open(tmp.path()).unwrap();
        assert_eq!(store.size_bytes(), blob.len() as u64);

        let mut buf = vec![0u8; blob.len()];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, blob);

        let mut mid = vec![0xAA; 1024 * 1024];
        store.read_at(1024 * 1024, &mut mid).unwrap();
        assert!(mid.iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_non_vhdx() {
        let tmp = write_tmp(b"not a vhdx file at all............");
        assert!(Vhdx::open(tmp.path()).is_err());
    }

    proptest! {
        #[test]
        fn reads_match_reference(
            seed in any::<u64>(),
            block in 0usize..3,
            within in 0usize..(1024 * 1024),
            len in 0usize..4096,
        ) {
            let mb = 1024 * 1024;
            let mut blob = vec![0u8; mb * 3];
            let mut x = seed | 1;
            for (i, b) in blob.iter_mut().enumerate() {
                let blk = i / mb;
                if blk == 1 && seed & 1 == 0 {
                    continue; // hole
                }
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                *b = (x & 0xFF) as u8;
            }
            let tmp = write_tmp(&fixtures::dynamic_vhdx(&blob));
            let store = Vhdx::open(tmp.path()).unwrap();

            let off = (block * mb + within).min(blob.len());
            let end = (off + len).min(blob.len());
            let mut buf = vec![0u8; end - off];
            store.read_at(off as u64, &mut buf).unwrap();
            prop_assert_eq!(&buf[..], &blob[off..end]);
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p xboot-core vhdx::`
Expected: PASS (unit tests + property test).

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/storage/vhdx
git commit -m "feat(storage): dynamic VHDX backing store with read property test"
```

---

## Task 15: `open_backing` factory (format detection)

**Files:**
- Modify: `crates/xboot-core/src/storage/mod.rs`

- [ ] **Step 1: Write the failing test**

Append a `#[cfg(test)]` test module to `crates/xboot-core/src/storage/mod.rs` (after the existing `tests` module — or merge into it). Add this new module:
```rust
#[cfg(test)]
mod factory_tests {
    use super::*;
    use crate::storage::vhd::fixtures as vhd_fix;
    use crate::storage::vhdx::fixtures as vhdx_fix;

    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xboot-bs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn detects_raw() {
        let p = write_tmp("a.raw", &[1u8; 2048]);
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 2048);
    }

    #[test]
    fn detects_fixed_vhd() {
        let p = write_tmp("a.vhd", &vhd_fix::fixed_vhd(&[2u8; 1024]));
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 1024);
    }

    #[test]
    fn detects_dynamic_vhd() {
        let p = write_tmp("b.vhd", &vhd_fix::dynamic_vhd(&[3u8; 8192], 4096));
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 8192);
    }

    #[test]
    fn detects_vhdx() {
        let p = write_tmp("a.vhdx", &vhdx_fix::dynamic_vhdx(&[4u8; 1024 * 1024]));
        let store = open_backing(&p).unwrap();
        assert_eq!(store.size_bytes(), 1024 * 1024);
    }
}
```

To make the VHD/VHDX fixtures reachable from `storage::mod` tests, the `vhd` and `vhdx` modules must expose their `fixtures` submodule to the crate under test. They already declare `#[cfg(test)] pub(crate) mod fixtures;` (Tasks 9 and 14), so no change is needed there.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core factory_tests::`
Expected: FAIL — `open_backing` still returns the "not implemented" error.

- [ ] **Step 3: Replace the `open_backing` stub with real detection**

In `crates/xboot-core/src/storage/mod.rs`, replace the stub `open_backing` function with:
```rust
/// Open a master image, detecting the format by magic bytes:
/// `vhdxfile` -> VHDX; trailing `conectix` footer -> VHD (fixed/dynamic);
/// otherwise raw.
pub fn open_backing(path: &Path) -> io::Result<Box<dyn BackingStore>> {
    use file_ext::read_exact_at;

    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();

    // VHDX: 8-byte identifier at offset 0.
    if len >= 8 {
        let mut ident = [0u8; 8];
        read_exact_at(&file, 0, &mut ident)?;
        if &ident == b"vhdxfile" {
            return Ok(Box::new(Vhdx::open(path)?));
        }
    }

    // VHD: 8-byte `conectix` cookie in the trailing 512-byte footer.
    if len >= 512 {
        let mut cookie = [0u8; 8];
        read_exact_at(&file, len - 512, &mut cookie)?;
        if &cookie == b"conectix" {
            return Vhd::open(path);
        }
    }

    // Fallback: treat as a raw image.
    Ok(Box::new(RawFile::open(path)?))
}
```

This needs `Vhd` and `Vhdx` in scope. Ensure the top of `mod.rs` re-exports them (already added in Task 1: `pub use vhd::Vhd; pub use vhdx::Vhdx;`). `Vhd::open` already returns `Box<dyn BackingStore>`, so it is returned directly.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p xboot-core factory_tests::`
Expected: PASS (all four).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/storage/mod.rs
git commit -m "feat(storage): open_backing format detection factory"
```

---

## Task 16: Robustness — randomized "no panic" smoke tests + cargo-fuzz harnesses

The spec (§7) wants the parsers to never panic on garbage. cargo-fuzz needs nightly + libFuzzer (not in stable CI), so we add **both**: cheap randomized smoke tests that run in CI on stable, and proper fuzz harnesses for deeper local runs.

**Files:**
- Modify: `crates/xboot-core/src/storage/mod.rs` (add a randomized smoke test module)
- Create: `fuzz/Cargo.toml`, `fuzz/fuzz_targets/vhd_footer.rs`, `fuzz/fuzz_targets/open_backing.rs`
- Modify: `crates/xboot-core/src/storage/vhd/footer.rs` and `crates/xboot-core/src/storage/vhdx/structs.rs` to mark the needed parsers `pub` for the fuzz crate

- [ ] **Step 1: Add a randomized smoke test (stable, runs in CI)**

Append to `crates/xboot-core/src/storage/mod.rs`:
```rust
#[cfg(test)]
mod fuzz_smoke {
    use super::*;

    // Deterministic xorshift so failures reproduce from the printed seed.
    fn fill(seed: u64, buf: &mut [u8]) {
        let mut x = seed | 1;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x & 0xFF) as u8;
        }
    }

    #[test]
    fn open_backing_never_panics_on_garbage() {
        for seed in 0..512u64 {
            let mut bytes = vec![0u8; 4096];
            fill(seed, &mut bytes);
            // Plant magic bytes some of the time to drive the parsers deeper.
            match seed % 3 {
                0 => bytes[0..8].copy_from_slice(b"vhdxfile"),
                1 => bytes[4096 - 512..4096 - 512 + 8].copy_from_slice(b"conectix"),
                _ => {}
            }
            let dir = std::env::temp_dir().join(format!("xboot-fuzz-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("g{seed}.img"));
            std::fs::write(&p, &bytes).unwrap();
            // Must return Ok or Err, never panic.
            let _ = open_backing(&p);
        }
    }
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p xboot-core fuzz_smoke::`
Expected: PASS (no panics).

- [ ] **Step 3: Expose the pure parsers for the fuzz crate**

The fuzz crate is a separate package that depends on `xboot-core`, so it can only call `pub` items. Make the two slice parsers public (keep them out of the prelude by not re-exporting from `lib.rs`):

In `crates/xboot-core/src/storage/vhd/footer.rs`, change `pub(crate) struct VhdFooter` → `pub struct VhdFooter` and `pub(crate) fn parse` → `pub fn parse`. In `crates/xboot-core/src/storage/vhd/mod.rs`, change `mod footer;` → `pub mod footer;`.

In `crates/xboot-core/src/storage/vhdx/structs.rs`, change `pub(crate) fn parse_region_table` → `pub fn parse_region_table` and `pub(crate) struct Regions`/its fields → `pub`. In `crates/xboot-core/src/storage/vhdx/mod.rs`, change `mod structs;` → `pub mod structs;`.

Run `cargo build -p xboot-core` and `cargo clippy -p xboot-core --all-targets` to confirm nothing else broke.

- [ ] **Step 4: Create the fuzz crate (excluded from the workspace in Task 1)**

`fuzz/Cargo.toml`:
```toml
[package]
name = "xboot-fuzz"
version = "0.0.0"
publish = false
edition = "2021"

[package.metadata]
cargo-fuzz = true

[dependencies]
libfuzzer-sys = "0.4"
xboot-core = { path = "../crates/xboot-core" }

[[bin]]
name = "vhd_footer"
path = "fuzz_targets/vhd_footer.rs"
test = false
doc = false

[[bin]]
name = "open_backing"
path = "fuzz_targets/open_backing.rs"
test = false
doc = false
```

`fuzz/fuzz_targets/vhd_footer.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use xboot_core::storage::vhd::footer::VhdFooter;

fuzz_target!(|data: &[u8]| {
    let _ = VhdFooter::parse(data);
});
```

`fuzz/fuzz_targets/open_backing.rs`:
```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    // open_backing takes a path; write the input to a temp file first.
    if let Ok(mut f) = tempfile::NamedTempFile::new() {
        if f.write_all(data).is_ok() {
            let _ = xboot_core::storage::open_backing(f.path());
        }
    }
});
```

The `open_backing` target also needs `tempfile` — add it under `[dependencies]` in `fuzz/Cargo.toml`:
```toml
tempfile = "3"
```

For these `pub` paths to resolve from another crate, `crates/xboot-core/src/storage/mod.rs` must expose the `vhd` and `vhdx` modules publicly. Change `mod vhd;` → `pub mod vhd;` and `mod vhdx;` → `pub mod vhdx;` in `mod.rs`.

- [ ] **Step 5: Verify the fuzz crate builds (stable build check; running needs nightly)**

Run: `cargo build --manifest-path fuzz/Cargo.toml` — this may fail if `cargo-fuzz`/libFuzzer toolchain isn't installed; that's acceptable. The authoritative check is, when available:
Run: `cargo +nightly fuzz build`
Expected: both fuzz targets compile.

Document the run command in the plan output for whoever has nightly:
```bash
cargo +nightly fuzz run vhd_footer -- -max_total_time=60
cargo +nightly fuzz run open_backing -- -max_total_time=60
```

- [ ] **Step 6: Commit**

```bash
git add fuzz crates/xboot-core/src/storage
git commit -m "test(storage): randomized smoke tests + cargo-fuzz harnesses for parsers"
```

---

## Task 17: Optional qemu-img interop tests (validate against the real format)

Our hand-rolled fixtures are self-consistent with our readers, which proves internal correctness but not conformance to real Microsoft images. This task cross-checks against images produced by `qemu-img`, skipping cleanly when it isn't installed (so CI stays green).

**Files:**
- Create: `crates/xboot-core/tests/backing_interop.rs`

- [ ] **Step 1: Write the interop test**

Create `crates/xboot-core/tests/backing_interop.rs`:
```rust
//! Interop: read images produced by `qemu-img` and compare against the source
//! bytes. Skipped automatically if `qemu-img` is not on PATH.

use std::process::Command;

fn qemu_img_available() -> bool {
    Command::new("qemu-img")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a raw source file of `size` bytes with deterministic content.
fn make_source(dir: &std::path::Path, size: usize) -> (std::path::PathBuf, Vec<u8>) {
    let mut data = vec![0u8; size];
    let mut x: u64 = 0x1234_5678;
    for b in data.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xFF) as u8;
    }
    let src = dir.join("src.raw");
    std::fs::write(&src, &data).unwrap();
    (src, data)
}

fn convert(src: &std::path::Path, out: &std::path::Path, fmt: &str, extra: &[&str]) {
    let status = Command::new("qemu-img")
        .args(["convert", "-f", "raw", "-O", fmt])
        .args(extra)
        .arg(src)
        .arg(out)
        .status()
        .unwrap();
    assert!(status.success(), "qemu-img convert to {fmt} failed");
}

fn assert_reads_match(image: &std::path::Path, expected: &[u8]) {
    let store = xboot_core::storage::open_backing(image).unwrap();
    assert_eq!(store.size_bytes(), expected.len() as u64);
    let mut buf = vec![0u8; expected.len()];
    store.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, expected, "image bytes differ from source");
}

#[test]
fn qemu_dynamic_vhd_matches_source() {
    if !qemu_img_available() {
        eprintln!("skipping: qemu-img not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, data) = make_source(dir.path(), 8 * 1024 * 1024);
    let out = dir.path().join("dyn.vhd");
    // subformat=dynamic, force_size keeps the virtual size exact.
    convert(&src, &out, "vpc", &["-o", "subformat=dynamic,force_size=on"]);
    assert_reads_match(&out, &data);
}

#[test]
fn qemu_fixed_vhd_matches_source() {
    if !qemu_img_available() {
        eprintln!("skipping: qemu-img not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, data) = make_source(dir.path(), 4 * 1024 * 1024);
    let out = dir.path().join("fixed.vhd");
    convert(&src, &out, "vpc", &["-o", "subformat=fixed,force_size=on"]);
    assert_reads_match(&out, &data);
}

#[test]
fn qemu_vhdx_matches_source() {
    if !qemu_img_available() {
        eprintln!("skipping: qemu-img not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, data) = make_source(dir.path(), 8 * 1024 * 1024);
    let out = dir.path().join("img.vhdx");
    convert(&src, &out, "vhdx", &["-o", "subformat=dynamic"]);
    assert_reads_match(&out, &data);
}
```

- [ ] **Step 2: Run the interop tests**

Run: `cargo test -p xboot-core --test backing_interop`
Expected: if `qemu-img` is installed, all three PASS; otherwise each prints "skipping" and passes.

If a test fails with `qemu-img` present, that means our parser diverges from real images — treat it as a bug in the relevant parser task (debug with superpowers:systematic-debugging), not a test problem.

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/tests/backing_interop.rs
git commit -m "test(storage): qemu-img interop tests (skipped when absent)"
```

---

## Task 18: Lint, format, full suite, done criteria

**Files:** none (verification + fixes only)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 2: Clippy (the CI gate is `-D warnings`)**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. Fix anything it finds (common ones here: needless range loops — prefer iterators; `len() == 0` → `is_empty()`; unused imports in test modules).

- [ ] **Step 3: Full test suite**

Run: `cargo test --all`
Expected: PASS — all VHD/VHDX/raw unit tests, both `proptest` read-equivalence properties, the factory tests, the randomized smoke test, and the interop tests (skipped or passing).

- [ ] **Step 4: Confirm the fuzz build (if nightly + cargo-fuzz available)**

Run: `cargo +nightly fuzz build`
Expected: both targets compile. (Skip if the toolchain isn't installed.)

- [ ] **Step 5: Commit any fixups**

```bash
git add -A
git commit -m "chore(storage): clippy/fmt cleanup for phase 02"
```

---

## Done criteria for Phase 02

- `open_backing(path)` returns a working `Box<dyn BackingStore>` for raw, fixed VHD, dynamic VHD, and dynamic VHDX images, detected by magic bytes.
- Reads are byte-correct: unallocated/holey regions read as zeros; arbitrary `(offset, len)` ranges spanning block and sector boundaries are handled.
- The master is opened strictly read-only (only `File::open`; no write paths exist on these types).
- `proptest` read-equivalence holds for both dynamic VHD and dynamic VHDX (reads == raw reference blob).
- Parsers reject corrupt input (bad cookie/signature/checksum/CRC, differencing images, dirty VHDX log, out-of-bounds tables) with a clear error and never panic — confirmed by the randomized smoke test and the cargo-fuzz harnesses.
- When `qemu-img` is available, our readers match real Microsoft-format images.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all` are green.

## Out of scope (deferred to later phases / not in v1)

- Writing to images (writes go to the COW overlay — Phase 03).
- Differencing VHD/VHDX (parent chains), VHDX log replay, partially-present VHDX blocks (state 7), non-512 logical sectors beyond what metadata reports.
- Caching of hot blocks (Phase 04) — these readers always hit the file.
- Performance tuning (the dynamic-VHD path reads the bitmap per sector; fine for v1, revisit if profiling shows it matters).
```
