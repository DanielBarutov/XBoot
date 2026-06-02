# Phase 01 — Scaffold & Foundations — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the XBoot Cargo workspace with a typed, validated TOML config loader, stub storage/network traits, a runnable binary, and green CI — the foundation every later phase builds on.

**Architecture:** Cargo workspace = `xboot-core` (library: config model + parsing + validation + trait definitions) and `xboot` (binary: argument handling, logging, calls into core). Config is parsed with `serde`/`toml` into typed structs, then validated (referential integrity + disk-type/role match + MAC format). Storage (`BackingStore`) and network (`NetIo`) are defined as traits only — implementations arrive in phases 02 and 06.

**Tech Stack:** Rust (edition 2021), `tokio`, `serde`, `toml`, `thiserror`, `tracing`/`tracing-subscriber`, `tempfile` (dev), GitHub Actions CI.

**Spec / roadmap:** `docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md` (§5 config), `docs/superpowers/plans/2026-06-02-xboot-roadmap.md` (Phase 01).

---

## File structure created in this phase

| File | Responsibility |
|---|---|
| `Cargo.toml` | Workspace root: members + shared dep versions |
| `.gitignore` | Ignore `/target` |
| `rustfmt.toml` | Formatting config |
| `crates/xboot-core/Cargo.toml` | Core library manifest |
| `crates/xboot-core/src/lib.rs` | Module wiring (`config`, `storage`, `net`) |
| `crates/xboot-core/src/config/mod.rs` | Re-exports of the config API |
| `crates/xboot-core/src/config/size.rs` | `ByteSize` + `parse_size` (human sizes like `8GB`) |
| `crates/xboot-core/src/config/model.rs` | Config structs/enums (`Config`, `Disk`, `Client`, …) |
| `crates/xboot-core/src/config/validate.rs` | `validate`, `is_valid_mac`, error types |
| `crates/xboot-core/src/config/load.rs` | `load_from_path` + `LoadError` |
| `crates/xboot-core/src/storage/mod.rs` | `BackingStore` trait (stub) |
| `crates/xboot-core/src/net/mod.rs` | `NetIo` trait (stub) |
| `crates/xboot/Cargo.toml` | Binary manifest |
| `crates/xboot/src/main.rs` | CLI entry: parse arg, init tracing, load + print config |
| `crates/xboot/tests/cli.rs` | Integration test of the binary |
| `.github/workflows/ci.yml` | fmt + clippy + test |

---

## Task 1: Workspace skeleton compiles

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `rustfmt.toml`
- Create: `crates/xboot-core/Cargo.toml`, `crates/xboot-core/src/lib.rs`
- Create: `crates/xboot/Cargo.toml`, `crates/xboot/src/main.rs`

- [ ] **Step 1: Write the workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = ["crates/xboot-core", "crates/xboot"]

[workspace.package]
edition = "2021"
version = "0.1.0"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
toml = "0.8"
thiserror = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
tracing = "0.1"
tracing-subscriber = "0.3"
tempfile = "3"
```

- [ ] **Step 2: Write `.gitignore` and `rustfmt.toml`**

`.gitignore`:
```gitignore
/target
```

`rustfmt.toml`:
```toml
edition = "2021"
```

- [ ] **Step 3: Write `crates/xboot-core/Cargo.toml`**

```toml
[package]
name = "xboot-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
toml.workspace = true
thiserror.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 4: Write `crates/xboot-core/src/lib.rs`**

```rust
pub mod config;
pub mod net;
pub mod storage;
```

- [ ] **Step 5: Write `crates/xboot/Cargo.toml`**

```toml
[package]
name = "xboot"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "xboot"
path = "src/main.rs"

[dependencies]
xboot-core = { path = "../xboot-core" }
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

- [ ] **Step 6: Write a temporary `crates/xboot/src/main.rs`**

This is replaced in Task 7; for now it just needs to compile.

```rust
fn main() {
    println!("xboot");
}
```

To make `lib.rs` compile, the referenced modules must exist. Create empty placeholders:

`crates/xboot-core/src/config/mod.rs`:
```rust
// Filled in Tasks 2-7.
```
`crates/xboot-core/src/storage/mod.rs`:
```rust
// Filled in Task 6.
```
`crates/xboot-core/src/net/mod.rs`:
```rust
// Filled in Task 6.
```

- [ ] **Step 7: Verify it builds**

Run: `cargo build --all`
Expected: builds successfully (warnings about empty modules are fine).

- [ ] **Step 8: Commit**

```bash
git add .
git commit -m "chore: scaffold xboot cargo workspace"
```

---

## Task 2: Human-readable byte sizes (`parse_size` / `ByteSize`)

**Files:**
- Create/replace: `crates/xboot-core/src/config/size.rs`
- Modify: `crates/xboot-core/src/config/mod.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Set `crates/xboot-core/src/config/mod.rs` to:
```rust
mod size;

pub use size::{parse_size, ByteSize, SizeParseError};
```

Create `crates/xboot-core/src/config/size.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("8GB").unwrap(), 8 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("2tb").unwrap(), 2 * 1024u64.pow(4));
        assert_eq!(parse_size(" 4MB ").unwrap(), 4 * 1024 * 1024);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("1.5GB").is_err());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core size::`
Expected: FAIL — `parse_size` / `ByteSize` not found (does not compile).

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/config/size.rs` (above the `tests` module):
```rust
use serde::{Deserialize, Deserializer};

/// A size in bytes, deserialized from strings like `"8GB"`, `"512MB"`, `"1024"`.
/// Unit suffixes are powers of 1024 (KB = 1024, MB = 1024², GB = 1024³, TB = 1024⁴).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum SizeParseError {
    #[error("invalid size value: '{0}'")]
    Invalid(String),
    #[error("size value overflows u64")]
    Overflow,
}

/// Parse a human-readable size string into bytes.
pub fn parse_size(s: &str) -> Result<u64, SizeParseError> {
    let trimmed = s.trim();
    let upper = trimmed.to_ascii_uppercase();

    let (num_part, mult): (&str, u64) = if let Some(n) = upper.strip_suffix("TB") {
        (n, 1024u64.pow(4))
    } else if let Some(n) = upper.strip_suffix("GB") {
        (n, 1024u64.pow(3))
    } else if let Some(n) = upper.strip_suffix("MB") {
        (n, 1024u64.pow(2))
    } else if let Some(n) = upper.strip_suffix("KB") {
        (n, 1024)
    } else if let Some(n) = upper.strip_suffix('B') {
        (n, 1)
    } else {
        (upper.as_str(), 1)
    };

    let value: u64 = num_part
        .trim()
        .parse()
        .map_err(|_| SizeParseError::Invalid(trimmed.to_string()))?;

    value.checked_mul(mult).ok_or(SizeParseError::Overflow)
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_size(&s).map(ByteSize).map_err(serde::de::Error::custom)
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core size::`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/config
git commit -m "feat(config): parse human-readable byte sizes"
```

---

## Task 3: Config data model + TOML parsing

**Files:**
- Create: `crates/xboot-core/src/config/model.rs`
- Modify: `crates/xboot-core/src/config/mod.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Update `crates/xboot-core/src/config/mod.rs`:
```rust
mod model;
mod size;

pub use model::{
    Client, ClientDefaults, Config, Disk, DiskMode, DiskType, WritebackPolicy,
};
pub use size::{parse_size, ByteSize, SizeParseError};
```

Create `crates/xboot-core/src/config/model.rs` with the test first:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ByteSize;

    const SAMPLE: &str = r#"
[[disk]]
id = "win11-master"
type = "image"
backing = "D:/xboot/win11.vhdx"
mode = "readonly"
ram_cache = "8GB"

[[disk]]
id = "games-main"
type = "game"
backing = "E:/games.vhdx"
mode = "readonly"
ram_cache = "16GB"

[[disk]]
id = "wb-nvme"
type = "writeback"
backing = "F:/xboot/writeback/"
ram_cache = "4GB"
policy = "volatile"

[[client]]
mac = "AA:BB:CC:DD:EE:01"
name = "PC-01"
system = "win11-master"
games = ["games-main"]
writeback = "wb-nvme"

[client_defaults]
system = "win11-master"
games = ["games-main"]
writeback = "wb-nvme"
"#;

    #[test]
    fn parses_sample_config() {
        let cfg: Config = toml::from_str(SAMPLE).unwrap();

        assert_eq!(cfg.disks.len(), 3);
        assert_eq!(cfg.disks[0].id, "win11-master");
        assert_eq!(cfg.disks[0].disk_type, DiskType::Image);
        assert_eq!(cfg.disks[0].mode, Some(DiskMode::Readonly));
        assert_eq!(cfg.disks[0].ram_cache, ByteSize(8 * 1024 * 1024 * 1024));
        assert_eq!(cfg.disks[2].policy, Some(WritebackPolicy::Volatile));

        assert_eq!(cfg.clients.len(), 1);
        assert_eq!(cfg.clients[0].games, vec!["games-main".to_string()]);
        assert_eq!(cfg.clients[0].name.as_deref(), Some("PC-01"));

        assert!(cfg.client_defaults.is_some());
    }

    #[test]
    fn games_default_to_empty() {
        let cfg: Config = toml::from_str(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"

[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "img"
"#,
        )
        .unwrap();
        assert!(cfg.clients[0].games.is_empty());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core model::`
Expected: FAIL — `Config` and friends not found (does not compile).

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/config/model.rs` (above the `tests` module):
```rust
use serde::Deserialize;

use crate::config::ByteSize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskType {
    Image,
    Game,
    Writeback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskMode {
    Readonly,
    Readwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WritebackPolicy {
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Disk {
    pub id: String,
    #[serde(rename = "type")]
    pub disk_type: DiskType,
    pub backing: String,
    #[serde(default)]
    pub mode: Option<DiskMode>,
    pub ram_cache: ByteSize,
    #[serde(default)]
    pub policy: Option<WritebackPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Client {
    pub mac: String,
    #[serde(default)]
    pub name: Option<String>,
    pub system: String,
    #[serde(default)]
    pub games: Vec<String>,
    pub writeback: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClientDefaults {
    pub system: String,
    #[serde(default)]
    pub games: Vec<String>,
    pub writeback: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    #[serde(rename = "disk", default)]
    pub disks: Vec<Disk>,
    #[serde(rename = "client", default)]
    pub clients: Vec<Client>,
    #[serde(default)]
    pub client_defaults: Option<ClientDefaults>,
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core model::`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/config
git commit -m "feat(config): typed model + TOML parsing"
```

---

## Task 4: MAC address validation

**Files:**
- Create: `crates/xboot-core/src/config/validate.rs`
- Modify: `crates/xboot-core/src/config/mod.rs`

- [ ] **Step 1: Declare the module and write the failing test**

Add to `crates/xboot-core/src/config/mod.rs` (module list and re-exports):
```rust
mod model;
mod size;
mod validate;

pub use model::{
    Client, ClientDefaults, Config, Disk, DiskMode, DiskType, WritebackPolicy,
};
pub use size::{parse_size, ByteSize, SizeParseError};
pub use validate::is_valid_mac;
```

Create `crates/xboot-core/src/config/validate.rs` with the test first:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_mac() {
        assert!(is_valid_mac("AA:BB:CC:DD:EE:01"));
        assert!(is_valid_mac("aa:bb:cc:dd:ee:ff"));
        assert!(!is_valid_mac("AA:BB:CC:DD:EE"));
        assert!(!is_valid_mac("AABBCCDDEEFF"));
        assert!(!is_valid_mac("ZZ:BB:CC:DD:EE:01"));
        assert!(!is_valid_mac("AA:BBB:CC:DD:EE:01"));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p xboot-core validate::`
Expected: FAIL — `is_valid_mac` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/config/validate.rs`:
```rust
/// True if `s` is a MAC address of the form `XX:XX:XX:XX:XX:XX` (hex, colon-separated).
pub fn is_valid_mac(s: &str) -> bool {
    let octets: Vec<&str> = s.split(':').collect();
    octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p xboot-core validate::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/config
git commit -m "feat(config): MAC address validation"
```

---

## Task 5: Config validation (referential integrity + disk-type/role match)

**Files:**
- Modify: `crates/xboot-core/src/config/validate.rs`
- Modify: `crates/xboot-core/src/config/mod.rs`

- [ ] **Step 1: Add the failing tests**

Replace the `tests` module in `crates/xboot-core/src/config/validate.rs` with:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> crate::config::Config {
        toml::from_str(s).unwrap()
    }

    const VALID: &str = r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "g1"
type = "game"
backing = "y"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
games = ["g1"]
writeback = "wb"
"#;

    #[test]
    fn validates_mac() {
        assert!(is_valid_mac("AA:BB:CC:DD:EE:01"));
        assert!(!is_valid_mac("AABBCCDDEEFF"));
        assert!(!is_valid_mac("ZZ:BB:CC:DD:EE:01"));
    }

    #[test]
    fn valid_config_passes() {
        assert!(validate(&parse(VALID)).is_ok());
    }

    #[test]
    fn detects_unknown_disk() {
        let cfg = parse(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "missing-wb"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::UnknownDisk { reference, .. } if reference == "missing-wb"
        )));
    }

    #[test]
    fn detects_wrong_disk_type() {
        let cfg = parse(
            r#"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "wb"
writeback = "wb"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::WrongDiskType { expected: DiskType::Image, .. }
        )));
    }

    #[test]
    fn detects_duplicate_disk_id() {
        let cfg = parse(
            r#"
[[disk]]
id = "a"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "a"
type = "game"
backing = "y"
ram_cache = "1GB"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs.contains(&ValidationError::DuplicateDiskId("a".to_string())));
    }

    #[test]
    fn detects_invalid_mac() {
        let cfg = parse(
            r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "NOT-A-MAC"
system = "img"
writeback = "wb"
"#,
        );
        let errs = validate(&cfg).unwrap_err().0;
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::InvalidMac(m) if m == "NOT-A-MAC")));
    }

    #[test]
    fn validates_client_defaults() {
        let cfg = parse(
            r#"
[client_defaults]
system = "ghost"
writeback = "ghost"
"#,
        );
        assert!(validate(&cfg).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p xboot-core validate::`
Expected: FAIL — `validate`, `ValidationError`, `ValidationErrors` not found.

- [ ] **Step 3: Write the implementation**

Replace the head of `crates/xboot-core/src/config/validate.rs` (everything above the `tests` module) with:
```rust
use std::collections::HashMap;

use crate::config::{Config, Disk, DiskType};

/// True if `s` is a MAC address of the form `XX:XX:XX:XX:XX:XX` (hex, colon-separated).
pub fn is_valid_mac(s: &str) -> bool {
    let octets: Vec<&str> = s.split(':').collect();
    octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()))
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("duplicate disk id: {0}")]
    DuplicateDiskId(String),
    #[error("'{reference}' (referenced by {context}) is not a defined disk")]
    UnknownDisk { reference: String, context: String },
    #[error("disk '{id}' is {actual:?} but {context} requires {expected:?}")]
    WrongDiskType {
        id: String,
        expected: DiskType,
        actual: DiskType,
        context: String,
    },
    #[error("invalid MAC address: {0}")]
    InvalidMac(String),
}

/// Collection of validation errors with a readable multi-line `Display`.
#[derive(Debug, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "  - {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

fn check_ref(
    errors: &mut Vec<ValidationError>,
    by_id: &HashMap<&str, &Disk>,
    reference: &str,
    expected: DiskType,
    context: &str,
) {
    match by_id.get(reference) {
        None => errors.push(ValidationError::UnknownDisk {
            reference: reference.to_string(),
            context: context.to_string(),
        }),
        Some(disk) if disk.disk_type != expected => {
            errors.push(ValidationError::WrongDiskType {
                id: disk.id.clone(),
                expected,
                actual: disk.disk_type,
                context: context.to_string(),
            })
        }
        Some(_) => {}
    }
}

fn check_profile(
    errors: &mut Vec<ValidationError>,
    by_id: &HashMap<&str, &Disk>,
    system: &str,
    games: &[String],
    writeback: &str,
    who: &str,
) {
    check_ref(errors, by_id, system, DiskType::Image, &format!("{who}.system"));
    for g in games {
        check_ref(errors, by_id, g, DiskType::Game, &format!("{who}.games"));
    }
    check_ref(
        errors,
        by_id,
        writeback,
        DiskType::Writeback,
        &format!("{who}.writeback"),
    );
}

/// Validate referential integrity, disk-type/role consistency, and MAC formats.
pub fn validate(cfg: &Config) -> Result<(), ValidationErrors> {
    let mut errors: Vec<ValidationError> = Vec::new();

    let mut by_id: HashMap<&str, &Disk> = HashMap::new();
    for disk in &cfg.disks {
        if by_id.insert(disk.id.as_str(), disk).is_some() {
            errors.push(ValidationError::DuplicateDiskId(disk.id.clone()));
        }
    }

    for c in &cfg.clients {
        if !is_valid_mac(&c.mac) {
            errors.push(ValidationError::InvalidMac(c.mac.clone()));
        }
        let who = format!("client '{}'", c.name.as_deref().unwrap_or(&c.mac));
        check_profile(&mut errors, &by_id, &c.system, &c.games, &c.writeback, &who);
    }

    if let Some(d) = &cfg.client_defaults {
        check_profile(
            &mut errors,
            &by_id,
            &d.system,
            &d.games,
            &d.writeback,
            "client_defaults",
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors(errors))
    }
}
```

Note: this replaces the whole head of the file written in Task 4, including the earlier `is_valid_mac` — there must be exactly one definition of it afterward.

- [ ] **Step 4: Update `mod.rs` re-exports**

Set `crates/xboot-core/src/config/mod.rs` to (the `load` module is added in Task 7):
```rust
mod model;
mod size;
mod validate;

pub use model::{
    Client, ClientDefaults, Config, Disk, DiskMode, DiskType, WritebackPolicy,
};
pub use size::{parse_size, ByteSize, SizeParseError};
pub use validate::{is_valid_mac, validate, ValidationError, ValidationErrors};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p xboot-core validate::`
Expected: PASS (all validation tests).

- [ ] **Step 6: Commit**

```bash
git add crates/xboot-core/src/config
git commit -m "feat(config): validate disk references, types, and MAC format"
```

---

## Task 6: `BackingStore` and `NetIo` trait stubs

**Files:**
- Replace: `crates/xboot-core/src/storage/mod.rs`
- Replace: `crates/xboot-core/src/net/mod.rs`

- [ ] **Step 1: Write `storage/mod.rs` with a usability test**

```rust
use std::io;

/// A read-only source of disk-image bytes (raw, VHD, VHDX, ...).
/// Implementations are added in phase 02.
pub trait BackingStore: Send + Sync {
    /// Total virtual size in bytes.
    fn size_bytes(&self) -> u64;

    /// Read exactly `buf.len()` bytes starting at byte `offset` into `buf`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
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

- [ ] **Step 2: Write `net/mod.rs`**

```rust
use std::io;

/// Abstraction over raw packet send/receive, hiding Windows vs Linux differences.
/// Implementations are added in phase 06.
pub trait NetIo: Send + Sync {
    /// Receive the next inbound packet into `buf`; returns the number of bytes read.
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Send one raw packet.
    fn send(&self, packet: &[u8]) -> io::Result<()>;
}
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p xboot-core storage::`
Expected: PASS (`trait_is_object_safe_and_usable`).

- [ ] **Step 4: Commit**

```bash
git add crates/xboot-core/src/storage crates/xboot-core/src/net
git commit -m "feat(core): define BackingStore and NetIo trait stubs"
```

---

## Task 7: Config file loader + binary wiring

**Files:**
- Create: `crates/xboot-core/src/config/load.rs`
- Modify: `crates/xboot-core/src/config/mod.rs` (add `mod load;` + re-export)
- Replace: `crates/xboot/src/main.rs`
- Create: `crates/xboot/tests/cli.rs`

- [ ] **Step 1: Write the failing loader tests**

Create `crates/xboot-core/src/config/load.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "wb"
"#;

    #[test]
    fn loads_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, VALID).unwrap();
        let cfg = load_from_path(&path).unwrap();
        assert_eq!(cfg.disks.len(), 2);
        assert_eq!(cfg.clients.len(), 1);
    }

    #[test]
    fn missing_file_is_io_error() {
        let err = load_from_path(std::path::Path::new("/no/such/file.toml")).unwrap_err();
        assert!(matches!(err, LoadError::Io { .. }));
    }

    #[test]
    fn invalid_config_is_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(
            &path,
            r#"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "ghost"
writeback = "ghost"
"#,
        )
        .unwrap();
        let err = load_from_path(&path).unwrap_err();
        assert!(matches!(err, LoadError::Validation(_)));
    }

    #[test]
    fn malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "this is = = not toml").unwrap();
        let err = load_from_path(&path).unwrap_err();
        assert!(matches!(err, LoadError::Parse(_)));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p xboot-core load::`
Expected: FAIL — `load_from_path` / `LoadError` not found.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/xboot-core/src/config/load.rs`:
```rust
use std::path::Path;

use crate::config::{validate, Config, ValidationErrors};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config validation failed:\n{0}")]
    Validation(#[from] ValidationErrors),
}

/// Read, parse, and validate a config file. Returns a fully-checked [`Config`].
pub fn load_from_path(path: &Path) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let cfg: Config = toml::from_str(&text)?;
    validate(&cfg)?;
    Ok(cfg)
}
```

Now add the `load` module to `crates/xboot-core/src/config/mod.rs`. Add the line `mod load;` to the module declarations and `pub use load::{load_from_path, LoadError};` to the re-exports, so the file reads:
```rust
mod load;
mod model;
mod size;
mod validate;

pub use load::{load_from_path, LoadError};
pub use model::{
    Client, ClientDefaults, Config, Disk, DiskMode, DiskType, WritebackPolicy,
};
pub use size::{parse_size, ByteSize, SizeParseError};
pub use validate::{is_valid_mac, validate, ValidationError, ValidationErrors};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p xboot-core load::`
Expected: PASS (all four tests).

- [ ] **Step 5: Replace `crates/xboot/src/main.rs`**

```rust
use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let path = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: xboot <config.toml>");
            return ExitCode::from(2);
        }
    };

    match xboot_core::config::load_from_path(&path) {
        Ok(cfg) => {
            tracing::info!(
                disks = cfg.disks.len(),
                clients = cfg.clients.len(),
                "config loaded"
            );
            println!("{cfg:#?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 6: Write the binary integration test**

Create `crates/xboot/tests/cli.rs`:
```rust
use std::process::Command;

#[test]
fn exits_2_without_arguments() {
    let bin = env!("CARGO_BIN_EXE_xboot");
    let output = Command::new(bin).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn loads_a_valid_config_and_exits_0() {
    let bin = env!("CARGO_BIN_EXE_xboot");
    let dir = std::env::temp_dir().join(format!("xboot-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("c.toml");
    std::fs::write(
        &path,
        r#"
[[disk]]
id = "img"
type = "image"
backing = "x"
ram_cache = "1GB"
[[disk]]
id = "wb"
type = "writeback"
backing = "z"
ram_cache = "1GB"
policy = "volatile"
[[client]]
mac = "AA:BB:CC:DD:EE:01"
system = "img"
writeback = "wb"
"#,
    )
    .unwrap();

    let output = Command::new(bin).arg(&path).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
}
```

- [ ] **Step 7: Run the full test suite**

Run: `cargo test --all`
Expected: PASS (core unit tests + binary integration tests).

- [ ] **Step 8: Commit**

```bash
git add crates
git commit -m "feat: config file loader and xboot binary entrypoint"
```

---

## Task 8: CI workflow + lint/format clean

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Write the CI workflow**

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - name: Format
        run: cargo fmt --all -- --check
      - name: Clippy
        run: cargo clippy --all-targets --all-features -- -D warnings
      - name: Test
        run: cargo test --all
```

- [ ] **Step 2: Run the same checks locally and fix any issues**

Run: `cargo fmt --all`
Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: clippy reports no warnings (fix any it finds — e.g. remove the placeholder `_Client`/`_ClientDefaults` import block from Task 5 if it warns).

Run: `cargo test --all`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: format, clippy, and test on push/PR"
```

---

## Done criteria for Phase 01

- `cargo build --all`, `cargo test --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo fmt --all -- --check` all pass.
- `xboot <config.toml>` loads + validates a config and prints the parsed model; invalid configs print a readable error and exit non-zero; no argument exits 2.
- `BackingStore` and `NetIo` traits exist as documented stubs.
- CI is green.
