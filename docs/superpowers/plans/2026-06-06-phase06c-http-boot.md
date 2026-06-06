# Phase 06c — HTTP Boot Script Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement an HTTP server that receives `GET /boot.ipxe?mac=...` from iPXE, resolves the client by MAC, assembles the iSCSI target, registers it, and returns an iPXE boot script.

**Architecture:** A pure `ClientManager` (MAC→Client+disks resolution) is separated from an HTTP server on hyper 1.x. The boot script generator is a single pure function. On each request: resolve MAC → build `ScsiTarget` with COW Volume per LUN (using a shared `ArcStore` wrapper for the RO master) → register in `TargetRegistry` behind a Mutex → generate script → 200 OK.

**Tech Stack:** Rust, hyper 1.x (HTTP), tokio (async, rt-multi-thread), same dependencies as existing project.

**Spec:** `docs/superpowers/specs/2026-06-06-phase06c-http-boot-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/xboot-core/src/config/model.rs` | Modify | Add `http_bind: IpAddr`, `http_port: u16` to `BootConfig` |
| `crates/xboot-core/src/config/validate.rs` | Modify | Validate `http_port != 0` |
| `crates/xboot-core/Cargo.toml` | Modify | Add `hyper` dependency |
| `crates/xboot-core/src/lib.rs` | Modify | Add `pub mod manager;` |
| `crates/xboot-core/src/manager/mod.rs` | Create | `ClientManager` + `ArcStore` wrapper |
| `crates/xboot-core/src/net/mod.rs` | Modify | Add `pub mod http;` |
| `crates/xboot-core/src/net/http/mod.rs` | Create | HTTP server on hyper 1.x |
| `crates/xboot-core/src/net/http/boot_script.rs` | Create | iPXE script generator |

---

### Task 1: Add `http_bind` and `http_port` to `BootConfig`

**Files:**
- Modify: `crates/xboot-core/src/config/model.rs`

- [ ] **Step 1: Add fields to `BootConfig` struct and update tests**

Open `crates/xboot-core/src/config/model.rs`. Replace the `BootConfig` struct definition:

```rust
/// Optional `[boot]` section: enables the proxyDHCP / PXE network-boot service.
/// When absent, the DHCP server is not started.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BootConfig {
    /// Our IP — used for next-server (siaddr / option 66) and server-id (option 54).
    pub server_ip: Ipv4Addr,
    /// Listen address for the `:67` and `:4011` UDP sockets.
    pub bind: IpAddr,
    /// Listen address for the HTTP boot-script server (phase 06c).
    #[serde(default = "default_http_bind")]
    pub http_bind: IpAddr,
    /// Port for the HTTP boot-script server (phase 06c).
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// Directory from which TFTP files are served (phase 06b).
    pub tftp_root: PathBuf,
    /// TFTP file for legacy BIOS (arch 0x0000), relative to tftp_root.
    pub bios_filename: String,
    /// TFTP file for UEFI x64 (arch 0x0007/0x0009), served by 06b.
    pub uefi_filename: String,
    /// iPXE arm HTTP boot script; `?mac=...` is appended at runtime. Served by 06c.
    pub http_script_url: String,
}
```

Add the default functions before `impl Config {`:

```rust
fn default_http_bind() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}
fn default_http_port() -> u16 {
    80
}
```

Update the `parses_boot_section` test — add asserts for the new fields:

```rust
#[test]
fn parses_boot_section() {
    let cfg: Config = toml::from_str(
        r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
tftp_root       = "/srv/xboot/tftp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
    )
    .unwrap();
    let boot = cfg.boot.expect("boot section present");
    assert_eq!(boot.server_ip, Ipv4Addr::new(192, 168, 1, 10));
    assert_eq!(boot.bind, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    assert_eq!(boot.http_bind, IpAddr::V4(Ipv4Addr::UNSPECIFIED)); // default
    assert_eq!(boot.http_port, 80); // default
    assert_eq!(boot.tftp_root, PathBuf::from("/srv/xboot/tftp"));
    assert_eq!(boot.bios_filename, "undionly.kpxe");
    assert_eq!(boot.uefi_filename, "ipxe.efi");
    assert_eq!(boot.http_script_url, "http://192.168.1.10/boot.ipxe");
}
```

Add a test for custom http_bind/http_port:

```rust
#[test]
fn parses_custom_http_bind_and_port() {
    let cfg: Config = toml::from_str(
        r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
http_bind       = "127.0.0.1"
http_port       = 8080
tftp_root       = "/srv/xboot/tftp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
    )
    .unwrap();
    let boot = cfg.boot.unwrap();
    assert_eq!(boot.http_bind, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(boot.http_port, 8080);
}
```

- [ ] **Step 2: Run model tests**

Run: `cargo test -p xboot-core config::model -- --nocapture`
Expected: all 6 model tests PASS

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/config/model.rs
git commit -m "feat(config): add http_bind and http_port to BootConfig

Adds http_bind (default 0.0.0.0) and http_port (default 80) for the
phase 06c HTTP boot-script server. Fields have serde defaults so
existing TOML configs remain valid.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Validate `http_port` in config validation

**Files:**
- Modify: `crates/xboot-core/src/config/validate.rs`

- [ ] **Step 1: Add `HttpPortZero` variant to `ValidationError`**

Open `crates/xboot-core/src/config/validate.rs`. Add after the `EmptyBootField` variant:

```rust
#[error("boot.http_port must not be zero")]
HttpPortZero,
```

- [ ] **Step 2: Add port validation to `check_boot`**

At the top of the `check_boot` function, before the existing path checks:

```rust
fn check_boot(errors: &mut Vec<ValidationError>, boot: &crate::config::BootConfig) {
    if boot.http_port == 0 {
        errors.push(ValidationError::HttpPortZero);
    }
    // ... existing validation follows
```

- [ ] **Step 3: Add validation test**

Add after the existing `rejects_empty_http_script_url` test:

```rust
#[test]
fn rejects_zero_http_port() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = parse(&format!(
        r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
http_port       = 0
tftp_root       = "{}"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        tmp.path().display()
    ));
    let errs = validate(&cfg).unwrap_err().0;
    assert!(errs.iter().any(|e| matches!(e, ValidationError::HttpPortZero)));
}
```

- [ ] **Step 4: Run validation tests**

Run: `cargo test -p xboot-core config::validate -- --nocapture`
Expected: all validation tests PASS

- [ ] **Step 5: Fix any other tests broken by new field**

Run: `cargo test -p xboot-core -- --nocapture`
Look for `BootConfig` construction in tests that now fails due to missing `http_bind` / `http_port`. Add the two fields to each.

Search for files constructing `BootConfig`:

```bash
grep -rn 'BootConfig {' crates/xboot-core/src/ | grep -v model.rs | grep -v test
```

Will need to add `http_bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),` and `http_port: 80,` to:
- `crates/xboot-core/src/net/dhcp/server.rs` test `test_cfg()`
- `crates/xboot-core/src/net/dhcp/decide.rs` test `cfg()`

- [ ] **Step 6: Commit**

```bash
git add crates/xboot-core/src/config/validate.rs crates/xboot-core/src/net/dhcp/server.rs crates/xboot-core/src/net/dhcp/decide.rs
git commit -m "feat(config): validate http_port in BootConfig (reject zero)

Adds HttpPortZero variant and check in check_boot. Updates existing
dhcp tests to include the new fields in BootConfig constructors.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Add hyper dependency and scaffold `manager` module

**Files:**
- Modify: `crates/xboot-core/Cargo.toml`
- Modify: `crates/xboot-core/src/lib.rs`
- Create: `crates/xboot-core/src/manager/mod.rs`

- [ ] **Step 1: Add hyper and http-body-util to Cargo.toml**

Open `crates/xboot-core/Cargo.toml`. Add in `[dependencies]`:

```toml
hyper = { version = "1", features = ["server", "http1"] }
http-body-util = "0.1"
bytes = "1"
```

And in `[dev-dependencies]`, add reqwest:

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
```

- [ ] **Step 2: Add `pub mod manager;` to lib.rs**

Open `crates/xboot-core/src/lib.rs`. Add `pub mod manager;` after the existing modules:

```rust
pub mod cache;
pub mod config;
pub mod iscsi;
pub mod manager;
pub mod net;
pub mod storage;
pub mod volume;
```

- [ ] **Step 3: Create `manager/mod.rs` — full module with `ArcStore` wrapper and `ClientManager`**

```rust
//! Client manager (phase 06c): MAC → Client resolution + cached RO backing stores.
//! Pure logic, no I/O beyond opening backing stores at construction time.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::storage::BackingStore;

/// Resolved configuration for one client.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub mac: String,
    pub name: Option<String>,
    pub system_disk_id: String,
    pub game_disk_ids: Vec<String>,
    pub writeback_disk_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("client_defaults references unknown disk '{0}'")]
    UnknownDisk(String),
    #[error("failed to open backing store '{path}': {source}")]
    OpenBacking { path: String, source: io::Error },
}

/// Wraps an `Arc<dyn BackingStore>` into `Box<dyn BackingStore>` for Volume::new.
/// Lets many Volumes share the same read-only master via Arc.
pub struct ArcStore(pub Arc<dyn BackingStore>);

impl BackingStore for ArcStore {
    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.0.read_at(offset, buf)
    }
}

/// Maps MAC addresses to client configurations and caches opened RO backing stores.
pub struct ClientManager {
    clients: HashMap<String, Arc<ClientConfig>>,
    defaults: Option<Arc<ClientConfig>>,
    stores: HashMap<String, Arc<dyn BackingStore>>,
}

impl ClientManager {
    /// Build the manager from a validated Config. Opens every image+game backing
    /// store and caches them behind `Arc`.
    pub fn new(cfg: &crate::config::Config) -> Result<Self, BuildError> {
        let mut clients: HashMap<String, Arc<ClientConfig>> = HashMap::new();
        for c in &cfg.clients {
            let cc = Arc::new(ClientConfig {
                mac: c.mac.to_lowercase(),
                name: c.name.clone(),
                system_disk_id: c.system.clone(),
                game_disk_ids: c.games.clone(),
                writeback_disk_id: c.writeback.clone(),
            });
            clients.insert(c.mac.to_lowercase(), cc);
        }

        let defaults = cfg.client_defaults.as_ref().map(|d| {
            Arc::new(ClientConfig {
                mac: String::new(),
                name: None,
                system_disk_id: d.system.clone(),
                game_disk_ids: d.games.clone(),
                writeback_disk_id: d.writeback.clone(),
            })
        });

        let mut stores: HashMap<String, Arc<dyn BackingStore>> = HashMap::new();
        for disk in &cfg.disks {
            if disk.disk_type == crate::config::DiskType::Image
                || disk.disk_type == crate::config::DiskType::Game
            {
                let path = Path::new(&disk.backing);
                let store = crate::storage::open_backing(path).map_err(|source| {
                    BuildError::OpenBacking {
                        path: disk.backing.clone(),
                        source,
                    }
                })?;
                stores.insert(disk.id.clone(), Arc::from(store));
            }
        }

        Ok(Self {
            clients,
            defaults,
            stores,
        })
    }

    /// Resolve a MAC address to a client config. Falls back to `client_defaults`.
    pub fn resolve(&self, mac: &str) -> Option<&ClientConfig> {
        let key = mac.to_lowercase();
        self.clients
            .get(&key)
            .or(self.defaults.as_ref())
            .map(|arc| arc.as_ref())
    }

    /// Get a cached RO backing store by disk ID.
    pub fn get_store(&self, disk_id: &str) -> Option<Arc<dyn BackingStore>> {
        self.stores.get(disk_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BootConfig, Client, ClientDefaults, Config, Disk, DiskType};
    use crate::config::ByteSize;
    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_boot() -> BootConfig {
        BootConfig {
            server_ip: Ipv4Addr::new(192, 168, 1, 10),
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            http_bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            http_port: 80,
            tftp_root: "/tmp".into(),
            bios_filename: "undionly.kpxe".into(),
            uefi_filename: "ipxe.efi".into(),
            http_script_url: "http://192.168.1.10/boot.ipxe".into(),
        }
    }

    fn cfg_with_clients(img_path: &str, game_path: &str, wb_path: &str) -> Config {
        Config {
            disks: vec![
                Disk {
                    id: "img".into(),
                    disk_type: DiskType::Image,
                    backing: img_path.into(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "game1".into(),
                    disk_type: DiskType::Game,
                    backing: game_path.into(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "wb".into(),
                    disk_type: DiskType::Writeback,
                    backing: wb_path.into(),
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
            ],
            clients: vec![Client {
                mac: "aa:bb:cc:dd:ee:01".into(),
                name: Some("pc-01".into()),
                system: "img".into(),
                games: vec!["game1".into()],
                writeback: "wb".into(),
            }],
            client_defaults: None,
            boot: Some(test_boot()),
        }
    }

    fn make_raw(path: &std::path::Path, size: usize) {
        std::fs::write(path, &vec![0u8; size]).unwrap();
    }

    #[test]
    fn resolve_known_mac() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        let game = tmp.path().join("game.raw");
        make_raw(&img, 4096);
        make_raw(&game, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &game.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        let c = mgr.resolve("aa:bb:cc:dd:ee:01").unwrap();
        assert_eq!(c.name.as_deref(), Some("pc-01"));
        assert_eq!(c.system_disk_id, "img");
        assert_eq!(c.game_disk_ids, vec!["game1"]);
    }

    #[test]
    fn resolve_case_insensitive_mac() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        assert!(mgr.resolve("AA:BB:CC:DD:EE:01").is_some());
    }

    #[test]
    fn resolve_unknown_mac_without_defaults_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        assert!(mgr.resolve("ff:ff:ff:ff:ff:ff").is_none());
    }

    #[test]
    fn resolve_unknown_mac_falls_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let mut cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        cfg.client_defaults = Some(ClientDefaults {
            system: "img".into(),
            games: vec!["game1".into()],
            writeback: "wb".into(),
        });

        let mgr = ClientManager::new(&cfg).unwrap();
        let c = mgr.resolve("ff:ff:ff:ff:ff:ff").unwrap();
        assert_eq!(c.system_disk_id, "img");
    }

    #[test]
    fn get_store_returns_cached_backing() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        std::fs::write(&img, &[0xABu8; 4096]).unwrap();

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        let store = mgr.get_store("img").unwrap();
        assert_eq!(store.size_bytes(), 4096);
        let mut buf = [0u8; 4];
        store.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0xAB, 0xAB, 0xAB, 0xAB]);
    }

    #[test]
    fn get_store_unknown_id_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("img.raw");
        make_raw(&img, 4096);

        let cfg = cfg_with_clients(
            &img.display().to_string(),
            &img.display().to_string(),
            &tmp.path().display().to_string(),
        );
        let mgr = ClientManager::new(&cfg).unwrap();
        assert!(mgr.get_store("nonexistent").is_none());
    }

    #[test]
    fn arcstore_delegates_to_inner() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("arc.raw");
        std::fs::write(&img, &[0xCDu8; 512]).unwrap();

        let store: Arc<dyn BackingStore> = Arc::from(crate::storage::open_backing(&img).unwrap());
        let wrapper = ArcStore(store);
        assert_eq!(wrapper.size_bytes(), 512);
        let mut buf = [0u8; 2];
        wrapper.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0xCD, 0xCD]);
    }
}
```

- [ ] **Step 4: Build and run manager tests**

Run: `cargo test -p xboot-core manager -- --nocapture`
Expected: all 7 tests PASS

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/Cargo.toml crates/xboot-core/src/lib.rs crates/xboot-core/src/manager/mod.rs
git commit -m "feat(manager): ClientManager + ArcStore — MAC→Client resolution

ClientManager indexes clients by lowercase MAC, falls back to
client_defaults, and caches RO backing stores for image+game disks.
ArcStore wraps Arc<dyn BackingStore> into Box<dyn BackingStore>
so many Volumes can share one read-only master.

7 unit tests cover known/unknown/fallback MACs, store access, and
the ArcStore delegation wrapper.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Implement `BootScript` generator

**Files:**
- Create: `crates/xboot-core/src/net/http/boot_script.rs`

- [ ] **Step 1: Write the test first**

```rust
//! iPXE boot script generator (phase 06c).
//! One pure function — input IQN + server IP, output script text.

/// Generate an iPXE script that sanboots from the given iSCSI target.
pub fn generate(iqn: &str, server_ip: &str) -> String {
    format!(
        "#!ipxe\nset initiator-iqn {iqn}\nsanboot iscsi:{server_ip}::::{iqn}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_sanboot_script() {
        let script = generate("iqn.2026-06.dev.xboot:pc-01", "192.168.1.10");
        assert!(script.contains("#!ipxe"));
        assert!(script.contains("set initiator-iqn iqn.2026-06.dev.xboot:pc-01"));
        assert!(script.contains(
            "sanboot iscsi:192.168.1.10::::iqn.2026-06.dev.xboot:pc-01"
        ));
    }

    #[test]
    fn generates_script_with_mac_iqn() {
        let script = generate(
            "iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01",
            "10.0.0.1",
        );
        assert!(script.contains(
            "set initiator-iqn iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01"
        ));
        assert!(script.contains(
            "sanboot iscsi:10.0.0.1::::iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01"
        ));
    }

    #[test]
    fn output_ends_with_newline() {
        let script = generate("iqn.2026-06.dev.xboot:test", "1.2.3.4");
        assert!(script.ends_with('\n'));
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p xboot-core net::http::boot_script -- --nocapture`
Expected: all 3 PASS

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/net/http/boot_script.rs
git commit -m "feat(http): iPXE boot script generator

Pure function that produces a #!ipxe script with sanboot from IQN
and server IP. 3 snapshot tests verify output format.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Register `pub mod http` in `net/mod.rs`

**Files:**
- Create: `crates/xboot-core/src/net/http/mod.rs` (minimal, for test setup)
- Modify: `crates/xboot-core/src/net/mod.rs`

- [ ] **Step 1: Add `pub mod http;` to `net/mod.rs`**

Open `crates/xboot-core/src/net/mod.rs`. Add:

```rust
pub mod dhcp;
pub mod http;
pub mod tftp;
```

- [ ] **Step 2: Create `net/http/mod.rs` as a one-liner**

```rust
pub mod boot_script;
```

This minimal module just exposes `boot_script`. The HTTP server code will be added in Task 6.

- [ ] **Step 3: Build to verify the module tree works**

Run: `cargo build -p xboot-core`
Expected: compiles

- [ ] **Step 4: Commit**

```bash
git add crates/xboot-core/src/net/mod.rs crates/xboot-core/src/net/http/mod.rs
git commit -m "feat(http): register pub mod http in net module tree

Exposes boot_script submodule under net::http.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 6: Implement HTTP server on hyper 1.x with integration test

**Files:**
- Modify: `crates/xboot-core/src/net/http/mod.rs`

- [ ] **Step 1: Full implementation — replace `mod.rs` with server + test**

Replace the entire file content:

```rust
//! HTTP boot-script server (phase 06c): hyper 1.x listener, one endpoint,
//! MAC→Client resolution, ScsiTarget assembly, script generation.
//! Returns text/plain iPXE scripts.

pub mod boot_script;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::iscsi::registry::TargetRegistry;
use crate::iscsi::scsi::{LogicalUnit, ScsiTarget};
use crate::manager::{ArcStore, ClientManager};
use crate::volume::{RamOverlay, Volume};

/// Start the HTTP boot-script server. Binds to `boot.http_bind:boot.http_port`.
/// Runs until the listener is closed or errors.
pub async fn serve(
    cfg: Arc<Config>,
    manager: Arc<ClientManager>,
    registry: Arc<Mutex<TargetRegistry>>,
) -> std::io::Result<()> {
    let boot = cfg
        .boot
        .as_ref()
        .expect("http::serve called without [boot] section");
    let addr = SocketAddr::new(boot.http_bind, boot.http_port);
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, _peer) = listener.accept().await?;
        let cfg = cfg.clone();
        let manager = manager.clone();
        let registry = registry.clone();

        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                handle(req, cfg.clone(), manager.clone(), registry.clone())
            });
            if let Err(e) = http1::Builder::new()
                .serve_connection(stream, svc)
                .await
            {
                eprintln!("http: connection error: {e}");
            }
        });
    }
}

/// Generate an IQN for a client using its name (preferred) or MAC.
fn client_iqn(client: &crate::manager::ClientConfig) -> String {
    let suffix = client.name.as_deref().unwrap_or(&client.mac);
    format!("iqn.2026-06.dev.xboot:{suffix}")
}

/// Handle one HTTP request.
async fn handle(
    req: Request<Incoming>,
    cfg: Arc<Config>,
    manager: Arc<ClientManager>,
    registry: Arc<Mutex<TargetRegistry>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Only GET /boot.ipxe
    if req.method() != Method::GET || req.uri().path() != "/boot.ipxe" {
        return Ok(not_found());
    }

    // Parse ?mac=...
    let mac = req.uri().query().and_then(parse_mac_param);
    let mac = match mac {
        Some(m) => m,
        None => return Ok(bad_request("Missing 'mac' parameter")),
    };

    // Resolve MAC → client config.
    let client = match manager.resolve(&mac) {
        Some(c) => c.clone(),
        None => return Ok(not_found()),
    };

    let boot = cfg
        .boot
        .as_ref()
        .expect("boot section must be present");
    let iqn = client_iqn(&client);

    // Build ScsiTarget: LUN 0 = system, LUN 1.. = games.
    let mut luns: Vec<Option<LogicalUnit>> = Vec::new();

    if let Some(store) = manager.get_store(&client.system_disk_id) {
        let vol = Volume::new(Box::new(ArcStore(store)), Box::new(RamOverlay::new()));
        luns.push(Some(LogicalUnit::new(vol).with_serial(&iqn)));
    }

    for game_id in &client.game_disk_ids {
        if let Some(store) = manager.get_store(game_id) {
            let vol = Volume::new(Box::new(ArcStore(store)), Box::new(RamOverlay::new()));
            luns.push(Some(LogicalUnit::new(vol)));
        }
    }

    let target = ScsiTarget::new(luns);

    // Register in the iSCSI registry.
    {
        let mut reg = registry.lock().await;
        reg.insert(&iqn, target);
    }

    // Generate the boot script.
    let server_ip = boot.server_ip.to_string();
    let script = boot_script::generate(&iqn, &server_ip);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(script)))
        .unwrap())
}

/// Extract `mac=...` from a query string. Percent-decodes the value.
fn parse_mac_param(query: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("mac=") {
            return Some(decode_percent(value));
        }
    }
    None
}

/// Minimal percent-decoder for URL-encoded hex values like %3A.
fn decode_percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
            {
                out.push((hi << 4 | lo) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from("Not Found")))
        .unwrap()
}

fn bad_request(msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Full::new(Bytes::from(msg.to_string())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BootConfig, Client, Config, Disk, DiskType, ByteSize};
    use crate::net::http::boot_script;
    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_cfg(tmp_dir: &std::path::Path) -> Config {
        let img = tmp_dir.join("img.raw");
        let game = tmp_dir.join("game.raw");
        std::fs::write(&img, &vec![0u8; 4096]).unwrap();
        std::fs::write(&game, &vec![0u8; 4096]).unwrap();

        Config {
            disks: vec![
                Disk {
                    id: "img".into(),
                    disk_type: DiskType::Image,
                    backing: img.display().to_string(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "game1".into(),
                    disk_type: DiskType::Game,
                    backing: game.display().to_string(),
                    mode: None,
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
                Disk {
                    id: "wb".into(),
                    disk_type: DiskType::Writeback,
                    backing: tmp_dir.display().to_string(),
                    ram_cache: ByteSize(1024 * 1024),
                    policy: None,
                },
            ],
            clients: vec![Client {
                mac: "aa:bb:cc:dd:ee:01".into(),
                name: Some("pc-01".into()),
                system: "img".into(),
                games: vec!["game1".into()],
                writeback: "wb".into(),
            }],
            client_defaults: None,
            boot: Some(BootConfig {
                server_ip: Ipv4Addr::new(192, 168, 1, 10),
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                http_bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                http_port: 0, // OS picks a free port
                tftp_root: "/tmp".into(),
                bios_filename: "undionly.kpxe".into(),
                uefi_filename: "ipxe.efi".into(),
                http_script_url: "http://192.168.1.10/boot.ipxe".into(),
            }),
        }
    }

    /// Start the server on a random port, return the bound address.
    async fn start_server(
        cfg: Config,
        manager: Arc<ClientManager>,
        registry: Arc<Mutex<TargetRegistry>>,
    ) -> SocketAddr {
        let cfg = Arc::new(cfg);
        let boot = cfg.boot.as_ref().unwrap();
        let listener = TcpListener::bind(SocketAddr::new(boot.http_bind, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await.unwrap();
                let cfg = cfg.clone();
                let manager = manager.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        handle(req, cfg.clone(), manager.clone(), registry.clone())
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(stream, svc)
                        .await;
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn get_boot_ipxe_returns_200_with_script() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(Mutex::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager.clone(), registry.clone()).await;

        let url = format!(
            "http://{}/boot.ipxe?mac=aa:bb:cc:dd:ee:01",
            addr
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("#!ipxe"));
        assert!(body.contains("iqn.2026-06.dev.xboot:pc-01"));
        assert!(body.contains("sanboot iscsi:192.168.1.10"));
    }

    #[tokio::test]
    async fn missing_mac_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(Mutex::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let url = format!("http://{}/boot.ipxe", addr);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn unknown_mac_without_defaults_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(Mutex::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let url = format!(
            "http://{}/boot.ipxe?mac=ff:ff:ff:ff:ff:ff",
            addr
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn wrong_path_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(Mutex::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let url = format!("http://{}/other", addr);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn post_method_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(Mutex::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager, registry).await;

        let client = reqwest::Client::new();
        let url = format!("http://{}/boot.ipxe?mac=aa:bb:cc:dd:ee:01", addr);
        let resp = client.post(&url).send().await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn happy_path_registers_target_in_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_cfg(tmp.path());
        let manager = Arc::new(ClientManager::new(&cfg).unwrap());
        let registry = Arc::new(Mutex::new(TargetRegistry::new()));
        let addr = start_server(cfg, manager.clone(), registry.clone()).await;

        let url = format!(
            "http://{}/boot.ipxe?mac=aa:bb:cc:dd:ee:01",
            addr
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);

        // Verify the target was registered.
        let reg = registry.lock().await;
        assert!(reg.get("iqn.2026-06.dev.xboot:pc-01").is_some());
    }

    #[test]
    fn parse_mac_extracts_value() {
        assert_eq!(
            parse_mac_param("mac=aa:bb:cc:dd:ee:01"),
            Some("aa:bb:cc:dd:ee:01".into())
        );
        assert_eq!(
            parse_mac_param("other=foo&mac=11:22:33:44:55:66"),
            Some("11:22:33:44:55:66".into())
        );
        assert_eq!(parse_mac_param("mac="), Some(String::new()));
        assert_eq!(parse_mac_param(""), None);
        assert_eq!(parse_mac_param("foo=bar"), None);
    }

    #[test]
    fn parse_mac_decodes_percent_encoded_colon() {
        // Untested for now — real clients send raw colons.
        assert_eq!(
            parse_mac_param("mac=aa%3Abb%3Acc%3Add%3Aee%3A01"),
            Some("aa:bb:cc:dd:ee:01".into())
        );
    }

    #[test]
    fn decode_percent_handles_plain_and_encoded() {
        assert_eq!(decode_percent("hello"), "hello");
        assert_eq!(decode_percent("ab%3Acd"), "ab:cd");
        assert_eq!(decode_percent("%20"), " ");
        assert_eq!(decode_percent("%gg"), "%gg"); // invalid hex left as-is
    }

    #[test]
    fn client_iqn_uses_name_when_present() {
        let c = crate::manager::ClientConfig {
            mac: "aa:bb:cc:dd:ee:01".into(),
            name: Some("pc-01".into()),
            system_disk_id: "img".into(),
            game_disk_ids: vec![],
            writeback_disk_id: "wb".into(),
        };
        assert_eq!(client_iqn(&c), "iqn.2026-06.dev.xboot:pc-01");
    }

    #[test]
    fn client_iqn_falls_back_to_mac() {
        let c = crate::manager::ClientConfig {
            mac: "aa-bb-cc-dd-ee-01".into(),
            name: None,
            system_disk_id: "img".into(),
            game_disk_ids: vec![],
            writeback_disk_id: "wb".into(),
        };
        assert_eq!(
            client_iqn(&c),
            "iqn.2026-06.dev.xboot:aa-bb-cc-dd-ee-01"
        );
    }
}
```

- [ ] **Step 2: Run the integration tests**

Run: `cargo test -p xboot-core net::http -- --nocapture`
Expected: all 11 tests PASS (6 tokio integration, 5 unit)

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/net/http/mod.rs
git commit -m "feat(http): hyper 1.x boot-script server with integration tests

HTTP server on hyper 1.x: GET /boot.ipxe?mac=... resolves client,
builds ScsiTarget with shared-RO-master Volumes, registers target
in Arc<Mutex<TargetRegistry>>, and returns an iPXE sanboot script.

6 integration tests with reqwest cover happy path, 400/404 errors,
405 on POST, and target registration verification. 5 unit tests
cover MAC parsing, percent decoding, and IQN generation.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 7: Final verification — full test suite, clippy, fmt

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace -- --nocapture`
Expected: all tests PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings

- [ ] **Step 3: Run rustfmt**

Run: `cargo fmt -- --check`
Expected: no diff

- [ ] **Step 4: Run cargo-fuzz build for existing fuzz targets**

Run: `cargo +nightly fuzz build`
Expected: all fuzz targets build

- [ ] **Step 5: Commit any fixes or mark as final**

If no changes needed:
```bash
echo "Phase 06c complete: all tests pass, clippy clean, fmt clean"
```

If fixes needed, commit them with:
```bash
git add -A
git commit -m "chore: fix clippy/fmt nits after 06c implementation

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
