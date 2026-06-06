# Phase 07 — main() & Graceful Shutdown Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire 4 ready services (proxyDHCP, TFTP, HTTP, iSCSI) into a single process with `main()`, graceful shutdown via `CancellationToken`, and a Russian-language setup guide.

**Architecture:** `std::sync::RwLock` wraps `TargetRegistry` (synchronous reads in iSCSI `Connection::handle()`, write-once in HTTP handler). `tokio_util::sync::CancellationToken` propagates through all 4 service loops. `main.rs` uses `tokio::spawn` + `tokio::select!` for concurrent serving + `ctrl_c` shutdown.

**Tech Stack:** Rust, tokio, tokio-util (CancellationToken), std::sync::RwLock, hyper, same deps.

**Spec:** `docs/superpowers/specs/2026-06-06-phase07-main-and-shutdown-design.md`

**Key design decision:** `std::sync::RwLock` (not `tokio::sync::RwLock`) — `Connection::handle()` is synchronous and calls `registry.read().unwrap().get(iqn)`. Registry reads are microsecond-scale HashMap lookups, so blocking a tokio worker thread is harmless. HTTP writes use `registry.write().unwrap().insert()` — rare and equally fast.

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/xboot-core/src/config/model.rs` | Modify | Add `iscsi_port: u16` with serde default 3260 |
| `crates/xboot-core/src/config/validate.rs` | Modify | Validate `iscsi_port != 0` |
| `Cargo.toml` (workspace) | Modify | Add `tokio-util` to workspace deps |
| `crates/xboot-core/Cargo.toml` | Modify | Add `tokio-util.workspace = true` |
| `crates/xboot/Cargo.toml` | Modify | Add `tokio-util.workspace = true` |
| `crates/xboot-core/src/net/dhcp/server.rs` | Modify | Add `CancellationToken` param; tokio::select! on recv |
| `crates/xboot-core/src/net/tftp/server.rs` | Modify | Add `CancellationToken` param; tokio::select! on recv |
| `crates/xboot-core/src/net/http/mod.rs` | Modify | `Mutex`→`RwLock`; `CancellationToken` param |
| `crates/xboot-core/src/iscsi/transport.rs` | Modify | `Arc<TReg>`→`Arc<RwLock<TReg>>`; `CancellationToken` |
| `crates/xboot-core/src/iscsi/session/mod.rs` | Modify | `Arc<TReg>`→`Arc<RwLock<TReg>>` |
| `crates/xboot/src/main.rs` | Modify | Full main(): load config, init, spawn 4 services, ctrl_c |
| `GUIDE.md` | Create | Russian-language setup guide |

---

### Task 1: `iscsi_port` in BootConfig + validation

**Files:**
- Modify: `crates/xboot-core/src/config/model.rs`
- Modify: `crates/xboot-core/src/config/validate.rs`

- [ ] **Step 1: Add `iscsi_port` field to `BootConfig`**

Open `crates/xboot-core/src/config/model.rs`. Add a default function before `impl Config`:

```rust
fn default_iscsi_port() -> u16 {
    3260
}
```

Add the field inside `BootConfig`, after `http_port`:

```rust
/// Port for the iSCSI target (phase 07). Default: 3260.
#[serde(default = "default_iscsi_port")]
pub iscsi_port: u16,
```

- [ ] **Step 2: Add `HttpPortZero`-style validation**

Open `crates/xboot-core/src/config/validate.rs`. Add the error variant:

```rust
#[error("boot.iscsi_port must not be zero")]
IscsiPortZero,
```

Add the check in `check_boot`, right after the `http_port == 0` check:

```rust
if boot.iscsi_port == 0 {
    errors.push(ValidationError::IscsiPortZero);
}
```

Add a test:

```rust
#[test]
fn rejects_zero_iscsi_port() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = parse(&format!(
        r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
iscsi_port      = 0
tftp_root       = "{}"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
        tmp.path().display()
    ));
    let errs = validate(&cfg).unwrap_err().0;
    assert!(errs.iter().any(|e| matches!(e, ValidationError::IscsiPortZero)));
}
```

- [ ] **Step 3: Update model tests**

In `parses_boot_section`, add: `assert_eq!(boot.iscsi_port, 3260);`

Add a new test:

```rust
#[test]
fn parses_custom_iscsi_port() {
    let cfg: Config = toml::from_str(
        r#"
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
iscsi_port      = 4321
tftp_root       = "/srv/xboot/tftp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
"#,
    )
    .unwrap();
    assert_eq!(cfg.boot.unwrap().iscsi_port, 4321);
}
```

- [ ] **Step 4: Update all `BootConfig` constructors in tests**

Add `iscsi_port: 3260,` to every `BootConfig { ... }` literal across the codebase. Run:

`grep -rn 'BootConfig {' crates/xboot-core/src/ | grep -v model.rs`

Expected hits: `net/dhcp/server.rs`, `net/dhcp/decide.rs`, `net/http/mod.rs` test helpers, `manager/mod.rs` test helper.

Add `iscsi_port: 3260,` to each.

- [ ] **Step 5: Run tests**

Run: `cargo test -p xboot-core config::model config::validate -- --nocapture`
Expected: new tests PASS

Run: `cargo test -p xboot-core -- --nocapture`
Expected: all tests PASS after fixing constructors

- [ ] **Step 6: Commit**

```bash
git add crates/xboot-core/src/config/model.rs crates/xboot-core/src/config/validate.rs
# plus any test files that got BootConfig { ... } updates
git add -A
git commit -m "feat(config): add iscsi_port to BootConfig (default 3260)

Adds iscsi_port field for iSCSI target port configuration with
serde default 3260 and validation (reject zero). Updates all
BootConfig literals in tests.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: `tokio-util` dependency + `CancellationToken` in proxyDHCP

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/xboot-core/Cargo.toml`
- Modify: `crates/xboot/Cargo.toml`
- Modify: `crates/xboot-core/src/net/dhcp/server.rs`

- [ ] **Step 1: Add `tokio-util` to workspace deps**

In `/home/daniel/Xboot/Cargo.toml`, add after `tokio` line:

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
tokio-util = { version = "0.7", features = ["rt"] }
```

- [ ] **Step 2: Add `tokio-util` to both crates**

In `crates/xboot-core/Cargo.toml`, add in `[dependencies]`:

```toml
tokio-util.workspace = true
```

In `crates/xboot/Cargo.toml`, add after `tokio.workspace = true`:

```toml
tokio-util.workspace = true
```

- [ ] **Step 3: Add `CancellationToken` to `dhcp::server::serve()`**

Current signature:
```rust
pub async fn serve(cfg: BootConfig) -> std::io::Result<()>
```

New signature:
```rust
use tokio_util::sync::CancellationToken;

pub async fn serve(cfg: BootConfig, token: CancellationToken) -> std::io::Result<()>
```

Inside `serve()`, the existing code:
```rust
pub async fn serve(cfg: BootConfig) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    let proxy = bind_socket(cfg.bind, 67).await?;
    let bootsvc = bind_socket(cfg.bind, 4011).await?;
    tokio::select! {
        r = listener_loop(proxy, cfg.clone(), Role::Proxy) => r,
        r = listener_loop(bootsvc, cfg.clone(), Role::BootService) => r,
    }
}
```

New code (wraps each listener_loop in a CancellationToken-aware select):
```rust
pub async fn serve(cfg: BootConfig, token: CancellationToken) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    let proxy = bind_socket(cfg.bind, 67).await?;
    let bootsvc = bind_socket(cfg.bind, 4011).await?;
    tokio::select! {
        r = listener_loop(proxy, cfg.clone(), Role::Proxy) => r,
        r = listener_loop(bootsvc, cfg.clone(), Role::BootService) => r,
        _ = token.cancelled() => {
            tracing::info!("dhcp: shutting down");
            Ok(())
        }
    }
}
```

The `listener_loop` itself uses `sock.recv_from(&mut buf).await` — to make it cancellable, wrap in `tokio::select!`:

At the top of `pub(crate) async fn listener_loop(...)` add the import and change the loop body:

```rust
use tokio_util::sync::CancellationToken;
```

The `listener_loop` function needs the token too. Add parameter:

```rust
pub(crate) async fn listener_loop(
    sock: UdpSocket,
    cfg: Arc<BootConfig>,
    role: Role,
    token: CancellationToken,
) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            result = sock.recv_from(&mut buf) => {
                let (n, from) = result?;
                if let Some((reply, dest)) = handle_datagram(&buf[..n], &cfg, role, from) {
                    sock.send_to(&packet::encode(&reply), dest).await?;
                }
            }
            _ = token.cancelled() => {
                tracing::info!("dhcp listener ({role:?}): shutting down");
                return Ok(());
            }
        }
    }
}
```

And `serve()` becomes:
```rust
pub async fn serve(cfg: BootConfig, token: CancellationToken) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    let proxy = bind_socket(cfg.bind, 67).await?;
    let bootsvc = bind_socket(cfg.bind, 4011).await?;
    tokio::select! {
        r = listener_loop(proxy, cfg.clone(), Role::Proxy, token.child_token()) => r,
        r = listener_loop(bootsvc, cfg.clone(), Role::BootService, token.child_token()) => r,
        _ = token.cancelled() => Ok(()),
    }
}
```

- [ ] **Step 4: Update dhcp tests**

The test `two_stage_chainload_over_loopback` and `non_pxe_datagram_is_ignored` call `listener_loop`. Add a `CancellationToken::new()` argument:

```rust
use tokio_util::sync::CancellationToken;
// ...
let token = CancellationToken::new();
tokio::spawn(async move {
    let _ = listener_loop(sock, cfg, Role::Proxy, token).await;
});
```

- [ ] **Step 5: Build and test**

Run: `cargo build -p xboot-core`
Expected: compiles

Run: `cargo test -p xboot-core net::dhcp -- --nocapture`
Expected: dhcp tests PASS

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/xboot-core/Cargo.toml crates/xboot/Cargo.toml crates/xboot-core/src/net/dhcp/server.rs
git commit -m "feat(dhcp): add CancellationToken for graceful shutdown

Adds tokio-util to workspace, CancellationToken param to dhcp::serve
and listener_loop. tokio::select! on recv_from + token.cancelled()
allows clean shutdown.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: `CancellationToken` in TFTP server

**Files:**
- Modify: `crates/xboot-core/src/net/tftp/server.rs`

- [ ] **Step 1: Add `CancellationToken` to TFTP `serve()`**

The current `serve` is a free function at line 46:

```rust
pub async fn serve(cfg: TftpServer) -> io::Result<()> {
    let sock = UdpSocket::bind(SocketAddr::new(cfg.bind, 69)).await?;
    let cfg = Arc::new(cfg);
    let mut buf = [0u8; 4096];
    loop {
        let (n, client_addr) = sock.recv_from(&mut buf).await?;
        let datagram = buf[..n].to_vec();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let _ = handle_request(datagram, client_addr, &cfg).await;
        });
    }
}
```

Add `use tokio_util::sync::CancellationToken;` at the top. Then change the signature and wrap the recv in select!:

```rust
pub async fn serve(cfg: TftpServer, token: CancellationToken) -> io::Result<()> {
    let sock = UdpSocket::bind(SocketAddr::new(cfg.bind, 69)).await?;
    let cfg = Arc::new(cfg);
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            result = sock.recv_from(&mut buf) => {
                let (n, client_addr) = result?;
                let datagram = buf[..n].to_vec();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _ = handle_request(datagram, client_addr, &cfg).await;
                });
            }
            _ = token.cancelled() => {
                tracing::info!("tftp: shutting down");
                return Ok(());
            }
        }
    }
}
```

- [ ] **Step 2: Update TFTP test helper**

TFTP tests use `start_test_server()` (line 323 of server.rs). It spawns a manual loop, not the `serve()` function, so it doesn't need a CancellationToken. No changes needed in tests unless there are tests that call `serve()` directly — check with `grep 'serve(' crates/xboot-core/src/net/tftp/server.rs`.

- [ ] **Step 3: Run tests**

Run: `cargo test -p xboot-core net::tftp -- --nocapture`
Expected: all TFTP tests PASS

- [ ] **Step 4: Commit**

```bash
git add crates/xboot-core/src/net/tftp/server.rs
git commit -m "feat(tftp): add CancellationToken for graceful shutdown

tokio::select! on recv_from + token.cancelled() in the serve loop.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: `Mutex` → `RwLock` + `CancellationToken` in HTTP server

**Files:**
- Modify: `crates/xboot-core/src/net/http/mod.rs`

- [ ] **Step 1: Replace all `tokio::sync::Mutex` with `std::sync::RwLock`**

Import change:
```rust
// Remove: use tokio::sync::Mutex;
// Add:
use std::sync::RwLock;
```

Update all type annotations. `Arc<Mutex<TargetRegistry>>` → `Arc<RwLock<TargetRegistry>>`.

- [ ] **Step 2: Update `serve()` signature**

```rust
use tokio_util::sync::CancellationToken;

pub async fn serve(
    cfg: Arc<Config>,
    manager: Arc<ClientManager>,
    registry: Arc<RwLock<TargetRegistry>>,
    token: CancellationToken,
) -> std::io::Result<()> {
    let boot = cfg.boot.as_ref().expect("http::serve called without [boot] section");
    let addr = SocketAddr::new(boot.http_bind, boot.http_port);
    let listener = TcpListener::bind(addr).await?;

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _peer) = result?;
                let cfg = cfg.clone();
                let manager = manager.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        handle(req, cfg.clone(), manager.clone(), registry.clone())
                    });
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await
                    {
                        eprintln!("http: connection error: {e}");
                    }
                });
            }
            _ = token.cancelled() => {
                tracing::info!("http: shutting down");
                return Ok(());
            }
        }
    }
}
```

- [ ] **Step 3: Update `handle()` — `Mutex::lock` → `RwLock::write`**

In `handle()`, change:
```rust
// Old:
let mut reg = registry.lock().await;
reg.insert(&iqn, target);

// New:
registry.write().unwrap().insert(&iqn, target);
```

- [ ] **Step 4: Update `start_server()` test helper**

Change generic params from `Arc<Mutex<...>>` to `Arc<RwLock<...>>`, and in the spawn block remove `.await` from lock calls. The test helper doesn't use CancellationToken (integration tests).

- [ ] **Step 5: Update test assertions**

In `happy_path_registers_target_in_registry`:
```rust
// Old:
let reg = registry.lock().await;
assert!(reg.get("iqn.2026-06.dev.xboot:pc-01").is_some());

// New:
assert!(registry.read().unwrap().get("iqn.2026-06.dev.xboot:pc-01").is_some());
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p xboot-core net::http -- --nocapture`
Expected: all 14 HTTP tests PASS

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/net/http/mod.rs
git commit -m "feat(http): Mutex→RwLock + CancellationToken for graceful shutdown

Replaces tokio::sync::Mutex with std::sync::RwLock for TargetRegistry
access (synchronous, no async contagion). Adds CancellationToken to
serve() with tokio::select! on listener.accept().

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: `Arc<TReg>` → `Arc<RwLock<TReg>>` + `CancellationToken` in iSCSI

**Files:**
- Modify: `crates/xboot-core/src/iscsi/session/mod.rs`
- Modify: `crates/xboot-core/src/iscsi/transport.rs`

- [ ] **Step 1: Update `Connection` struct**

In `crates/xboot-core/src/iscsi/session/mod.rs`:

```rust
use std::sync::RwLock;

pub struct Connection {
    registry: Arc<RwLock<TargetRegistry>>,
    // ... rest unchanged
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
```

- [ ] **Step 2: Update registry reads in session module**

In `login.rs` — find `conn.registry.get(iqn)` and change to:

```rust
conn.target = conn.registry.read().unwrap().get(iqn);
```

In `mod.rs` — find `self.registry.iqns()` (used in SendTargets) and change to:

```rust
for iqn in self.registry.read().unwrap().iqns() {
```

- [ ] **Step 3: Update `transport.rs`**

```rust
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

pub async fn serve(
    listener: TcpListener,
    registry: Arc<RwLock<TargetRegistry>>,
    token: CancellationToken,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (sock, _peer) = result?;
                let reg = registry.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, reg).await;
                });
            }
            _ = token.cancelled() => {
                tracing::info!("iscsi: shutting down");
                return Ok(());
            }
        }
    }
}

async fn handle_conn(
    mut sock: TcpStream,
    registry: Arc<RwLock<TargetRegistry>>,
) -> std::io::Result<()> {
    let mut conn = Connection::new(registry);
    // ... rest unchanged
```

- [ ] **Step 4: Update transport tests**

Test helper `registry()` returns `Arc<RwLock<TargetRegistry>>`:

```rust
fn registry() -> Arc<RwLock<TargetRegistry>> {
    let vol = Volume::new(/*...*/);
    let mut reg = TargetRegistry::new();
    reg.insert(IQN, ScsiTarget::new(vec![Some(LogicalUnit::new(vol))]));
    Arc::new(RwLock::new(reg))
}
```

Same for `multi_registry()`.

In `loopback_login_write_read_logout` and `concurrent_clients_are_isolated`:

```rust
let token = CancellationToken::new();
tokio::spawn(async move { serve(listener, reg, token).await });
```

Use `tokio::spawn` with `let _ = tokio::spawn(...)` or `let handle = tokio::spawn(...)`.

- [ ] **Step 5: Run transport tests**

Run: `cargo test -p xboot-core iscsi::transport -- --nocapture`
Expected: loopback and concurrent tests PASS

- [ ] **Step 6: Run all iSCSI tests**

Run: `cargo test -p xboot-core iscsi -- --nocapture`
Expected: all iSCSI tests PASS

- [ ] **Step 7: Commit**

```bash
git add crates/xboot-core/src/iscsi/session/mod.rs crates/xboot-core/src/iscsi/transport.rs
git commit -m "feat(iscsi): Arc<TReg>→Arc<RwLock<TReg>> + CancellationToken

Replaces Arc<TargetRegistry> with Arc<RwLock<TargetRegistry>> in
Connection and transport. Uses synchronous read()/write() locks
since Connection::handle() is sync. Adds CancellationToken to
transport::serve() for graceful shutdown.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 6: Full `main.rs`

**Files:**
- Modify: `crates/xboot/src/main.rs`

- [ ] **Step 1: Replace `main.rs` entirely**

```rust
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use xboot_core::iscsi::registry::TargetRegistry;
use xboot_core::manager::ClientManager;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: xboot <config.toml>");
            return ExitCode::from(2);
        }
    };

    let cfg = match xboot_core::config::load_from_path(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let boot = match &cfg.boot {
        Some(b) => b.clone(),
        None => {
            tracing::error!("[boot] section is required");
            return ExitCode::FAILURE;
        }
    };

    // Build ClientManager — opens all RO backing stores.
    let manager = match ClientManager::new(&cfg) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let registry = Arc::new(RwLock::new(TargetRegistry::new()));
    let token = CancellationToken::new();
    let cfg = Arc::new(cfg);
    let iscsi_addr = SocketAddr::new(boot.bind, boot.iscsi_port);

    tracing::info!(
        server_ip = %boot.server_ip,
        dhcp_bind = %boot.bind,
        http_bind = %boot.http_bind,
        http_port = boot.http_port,
        iscsi_port = boot.iscsi_port,
        tftp_root = %boot.tftp_root.display(),
        "starting xboot services"
    );

    // proxyDHCP: :67 + :4011
    let dhcp = {
        let t = token.clone();
        tokio::spawn(async move {
            if let Err(e) = xboot_core::net::dhcp::server::serve(boot.clone(), t).await {
                tracing::error!("dhcp: {e}");
            }
        })
    };

    // TFTP: :69
    let tftp = {
        let t = token.clone();
        let srv = xboot_core::net::tftp::server::TftpServer::new(boot.bind, boot.tftp_root.clone());
        tokio::spawn(async move {
            if let Err(e) = xboot_core::net::tftp::server::serve(srv, t).await {
                tracing::error!("tftp: {e}");
            }
        })
    };

    // HTTP: boot.http_bind:boot.http_port
    let http = {
        let c = cfg.clone();
        let m = manager.clone();
        let r = registry.clone();
        let t = token.clone();
        tokio::spawn(async move {
            if let Err(e) = xboot_core::net::http::serve(c, m, r, t).await {
                tracing::error!("http: {e}");
            }
        })
    };

    // iSCSI: boot.bind:boot.iscsi_port
    let iscsi = {
        let r = registry.clone();
        let t = token.clone();
        tokio::spawn(async move {
            let listener = match TcpListener::bind(iscsi_addr).await {
                Ok(l) => {
                    tracing::info!("iscsi: listening on {iscsi_addr}");
                    l
                }
                Err(e) => {
                    tracing::error!("iscsi: bind {iscsi_addr}: {e}");
                    return;
                }
            };
            if let Err(e) = xboot_core::iscsi::transport::serve(listener, r, t).await {
                tracing::error!("iscsi: {e}");
            }
        })
    };

    // Ctrl+C → cancel all.
    let ctrl_c = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("SIGINT received, shutting down...");
        token.cancel();
    });

    // Wait for any service to exit (first error or ctrl_c).
    tokio::select! {
        _ = dhcp => {},
        _ = tftp => {},
        _ = http => {},
        _ = iscsi => {},
        _ = ctrl_c => {},
    }

    tracing::info!("xboot stopped");
    ExitCode::SUCCESS
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p xboot`
Expected: compiles

- [ ] **Step 3: Run CLI smoke tests**

Run: `cargo test -p xboot -- --nocapture`
Expected: 3 CLI tests PASS (or adapt if `loads_a_valid_config` breaks due to `[boot]` required)

Check if existing test config `tests/fixtures/valid.toml` needs `iscsi_port` — likely not since it has serde default.

If `loads_a_valid_config_and_exits_0` fails with "boot section required", update the test fixture to include `[boot]` section. But this test just verifies config loads — it shouldn't need `[boot]` in main because that check is in `main()`, not in `config::load_from_path`.

Wait — the CLI test `loads_a_valid_config_and_exits_0` calls the actual `xboot` binary with a config. If main now requires `[boot]`, the test fixture needs updating.

- [ ] **Step 4: Commit**

```bash
git add crates/xboot/src/main.rs
# If test fixtures changed:
git add crates/xboot/tests/
git commit -m "feat(main): full main() — 4 services + graceful shutdown

Loads config, builds ClientManager, spawns DHCP/TFTP/HTTP/iSCSI
services with CancellationToken, handles ctrl_c for clean shutdown.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 7: Fix all cascaded test breakage

**Files:** Various — depends on what broke from Tasks 1-6.

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace -- --nocapture 2>&1 | tail -80`
Identify all failures.

- [ ] **Step 2: Fix each failure systematically**

Common breakage patterns:

1. **Missing `iscsi_port` in `BootConfig { ... }`** — add `iscsi_port: 3260,`
2. **`Mutex` → `RwLock`** — change `.lock().await` to `.write().unwrap()` or `.read().unwrap()`
3. **`serve()` calls missing `token` arg** — add `CancellationToken::new()`
4. **CLI test fixtures** — ensure `tests/fixtures/valid.toml` has `[boot]` section or create a minimal one

- [ ] **Step 3: Run again until green**

Run: `cargo test --workspace -- --nocapture`
Expected: all tests PASS

- [ ] **Step 4: Commit fixups**

```bash
git add -A
git commit -m "test: fix all tests after Phase 07 signature changes

Add iscsi_port to BootConfig literals, Mutex→RwLock in test helpers,
CancellationToken in test serve() calls, update CLI test fixtures.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 8: `GUIDE.md` — русское руководство

**Files:**
- Create: `GUIDE.md`

- [ ] **Step 1: Write the guide**

```markdown
# XBoot — Руководство по установке и запуску

> **XBoot** — сервер бездисковой загрузки (альтернатива CCboot).  
> Клиентские ПК загружаются по сети через PXE/iSCSI, без локальных дисков.

---

## 1. Установка окружения

### 1.1 Установка Rust

XBoot написан на Rust. Установите компилятор через [rustup](https://rustup.rs):

**Linux:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
```

**Windows (PowerShell от администратора):**
```powershell
winget install Rustlang.Rustup
```

Проверьте установку:
```bash
rustc --version   # должно быть ≥ 1.80
cargo --version
```

### 1.2 Ночная сборка (опционально, для fuzz-тестов)

```bash
rustup toolchain install nightly
```

---

## 2. Компиляция

### 2.1 Клонирование репозитория

```bash
git clone https://github.com/your-org/xboot.git
cd xboot
```

### 2.2 Сборка

**Debug-сборка (для разработки):**
```bash
cargo build
```

Бинарник: `target/debug/xboot`

**Release-сборка (для продакшена):**
```bash
cargo build --release
```

Бинарник: `target/release/xboot`

### 2.3 Запуск тестов

```bash
cargo test --workspace
```

---

## 3. Структура конфигурации

XBoot использует один файл `config.toml`. Вот полный пример с комментариями:

```toml
# ── Диски (backing stores) ──────────────────────────────

[[disk]]
id        = "win11-master"        # ID диска — используется в секции клиентов
type      = "image"               # image | game | writeback
backing   = "/data/xboot/win11.vhdx"  # путь к VHD/VHDX или raw-файлу
ram_cache = "8GB"                 # RAM-кэш на этот диск (независимый LRU)

[[disk]]
id        = "games-main"
type      = "game"                # game-диски — отдельный LUN на клиенте
backing   = "/data/xboot/games.vhdx"
ram_cache = "16GB"

[[disk]]
id        = "wb-nvme"
type      = "writeback"           # сюда пишутся COW-оверлеи клиентов
backing   = "/data/xboot/writeback/"
ram_cache = "4GB"

# ── Клиенты ─────────────────────────────────────────────

[[client]]
mac       = "AA:BB:CC:DD:EE:01"
name      = "PC-01"               # отображаемое имя (опционально)
system    = "win11-master"        # LUN 0 — системный диск
games     = ["games-main"]        # LUN 1.. — игровые диски
writeback = "wb-nvme"             # куда писать COW-оверлей

# ── Профиль по умолчанию (для неизвестных MAC) ──────────

[client_defaults]
system    = "win11-master"
games     = ["games-main"]
writeback = "wb-nvme"

# ── Параметры сетевой загрузки ──────────────────────────

[boot]
server_ip       = "192.168.1.10"  # IP сервера (его видят клиенты)
bind            = "0.0.0.0"       # на каком IP слушать все сервисы
http_bind       = "0.0.0.0"       # HTTP-сервер (можно отдельный IP)
http_port       = 80              # порт HTTP boot-скрипта
iscsi_port      = 3260            # порт iSCSI (стандартный)
tftp_root       = "/srv/xboot/tftp"  # директория с файлами TFTP
bios_filename   = "undionly.kpxe"     # iPXE для BIOS
uefi_filename   = "ipxe.efi"          # iPXE для UEFI
http_script_url = "http://192.168.1.10/boot.ipxe"  # URL, который iPXE запросит
```

---

## 4. Подготовка файлов

### 4.1 Бинарники iPXE

Скачайте готовые бинарники iPXE:

- **BIOS**: `undionly.kpxe` — [https://boot.ipxe.org/undionly.kpxe](https://boot.ipxe.org/undionly.kpxe)
- **UEFI**: `ipxe.efi` — [https://boot.ipxe.org/ipxe.efi](https://boot.ipxe.org/ipxe.efi)

Положите их в директорию `tftp_root` (например, `/srv/xboot/tftp/`).

### 4.2 Образы дисков

XBoot поддерживает три формата:

- **`.vhdx`** — рекомендуется для Windows (динамический, Hyper-V совместимый)
- **`.vhd`** (fixed/dynamic) — для совместимости
- **`.raw`** — сырые образы (только для тестов)

**Системный образ (image):**
Подготовьте эталонную Windows-машину (см. отдельное руководство по инжекту iSCSI-boot
драйверов) и сохраните её как VHDX.

**Игровой диск (game):**
Общий read-only диск с играми. Может быть большим (1-4 ТБ VHDX).

Оба образа должны быть доступны **только на чтение** (физически read-only на уровне ОС).

### 4.3 Директория writeback

Создайте папку под временные COW-оверлеи (желательно на быстром NVMe):

```bash
mkdir -p /data/xboot/writeback
```

В v1 оверлеи volatile — сбрасываются при перезагрузке клиента.

---

## 5. Запуск

### 5.1 Права доступа

XBoot слушает привилегированные порты 67 (DHCP), 69 (TFTP) и 80 (HTTP). Запускайте от root
или настройте capabilities:

**Linux (рекомендуется — capabilities вместо root):**
```bash
sudo setcap 'cap_net_bind_service=+ep' target/release/xboot
```

Или запускайте от root:
```bash
sudo target/release/xboot config.toml
```

### 5.2 Запуск

```bash
xboot config.toml
```

Вы увидите примерно такой вывод:

```
2026-06-06T12:00:00.000Z  INFO xboot: starting xboot services
    server_ip = 192.168.1.10
    dhcp_bind = 0.0.0.0
    http_bind = 0.0.0.0
    http_port = 80
    iscsi_port = 3260
    tftp_root = /srv/xboot/tftp
2026-06-06T12:00:00.001Z  INFO iscsi: listening on 0.0.0.0:3260
```

### 5.3 Остановка

Нажмите `Ctrl+C` — все 4 сервиса остановятся:

```
2026-06-06T12:05:00.000Z  INFO SIGINT received, shutting down...
2026-06-06T12:05:00.001Z  INFO dhcp: shutting down
2026-06-06T12:05:00.001Z  INFO tftp: shutting down
2026-06-06T12:05:00.001Z  INFO http: shutting down
2026-06-06T12:05:00.001Z  INFO iscsi: shutting down
2026-06-06T12:05:00.002Z  INFO xboot stopped
```

### 5.4 systemd-сервис (Linux)

Создайте `/etc/systemd/system/xboot.service`:

```ini
[Unit]
Description=XBoot Diskless Boot Server
After=network.target

[Service]
Type=simple
ExecStart=/opt/xboot/xboot /etc/xboot/config.toml
Restart=always
RestartSec=5
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

Активируйте:
```bash
sudo systemctl enable --now xboot
sudo systemctl status xboot
```

---

## 6. Настройка сети

### 6.1 proxyDHCP

XBoot реализует **proxyDHCP** — он отвечает только на PXE-часть DHCP-запроса
(не выдаёт IP-адреса). Основной роутер/DHCP-сервер продолжает раздавать IP.

XBoot слушает порты 67 и 4011 (PXE Boot Server). Настройка сети не требуется —
просто убедитесь, что XBoot и клиенты в одной подсети.

**Важно:** если в сети уже есть PXE-сервер (например, WDS), выключите его
или настройте фильтрацию по MAC.

### 6.2 Клиентские ПК

В BIOS/UEFI клиента:
1. Включите **Network Boot** / **PXE Boot**
2. Установите сетевую карту первой в порядке загрузки
3. Отключите **Secure Boot** (некоторые сборки iPXE этого требуют)

### 6.3 Порядок загрузки

При включении клиента:
1. BIOS/UEFI шлёт DHCP-запрос с PXE-пометкой
2. Роутер выдаёт IP; XBoot (proxyDHCP) отвечает PXE-частью: «загрузчик у меня»
3. Клиент скачивает iPXE (`undionly.kpxe` / `ipxe.efi`) по TFTP
4. iPXE делает HTTP GET на `http://<server_ip>/boot.ipxe?mac=<MAC>`
5. XBoot генерирует boot-скрипт под этот MAC и регистрирует iSCSI target
6. iPXE подключает iSCSI-диск и передаёт управление Windows
7. Windows загружается через iBFT (iSCSI Boot Firmware Table)

---

## 7. Диагностика

### 7.1 Клиент не получает iPXE

Проверьте:
- XBoot запущен и слушает порты (`ss -tulnp | grep xboot`)
- Клиент и сервер в одной подсети (или DHCP relay настроен)
- В логах XBoot нет ошибок DHCP
- `tftp_root` существует и содержит бинарники iPXE

### 7.2 iPXE загружается, но не может получить boot-скрипт

Проверьте:
- `http_script_url` в конфиге указывает на правильный IP
- Порты HTTP (по умолчанию 80) открыты
- В логах `curl http://<server>/boot.ipxe?mac=AA:BB:CC:DD:EE:01`

### 7.3 iSCSI target не регистрируется

Проверьте:
- MAC клиента прописан в `[[client]]` или в `[client_defaults]`
- ID дисков (`system`, `games`, `writeback`) ссылаются на существующие `[[disk]]`
- Backing store (VHD/VHDX) существует и читается

### 7.4 Логирование

Установите переменную окружения `RUST_LOG` для детальных логов:

```bash
RUST_LOG=debug xboot config.toml       # все подсистемы
RUST_LOG=xboot_core::net=debug xboot config.toml  # только сеть
```

### 7.5 Типовые ошибки

| Ошибка | Причина | Решение |
|--------|---------|---------|
| `boot.http_port must not be zero` | не указан HTTP порт | добавьте `http_port = 80` |
| `boot.iscsi_port must not be zero` | не указан iSCSI порт | добавьте `iscsi_port = 3260` |
| `boot.tftp_root ... is not a directory` | директория TFTP не существует | создайте или укажите правильный путь |
| `failed to open backing store` | файл VHD/VHDX не найден | проверьте пути в `[[disk]]` |
| `unknown target IQN` (iSCSI) | клиент пытается подключиться без предварительного HTTP-запроса | убедитесь, что iPXE доходит до HTTP-запроса |
| `address in use` | порт занят другим процессом | проверьте `ss -tulnp`, освободите порт |

---

## 8. Дополнительные ресурсы

- [Спецификация XBoot](docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md)
- [iPXE документация](https://ipxe.org/docs)
- [RFC 7143 — iSCSI Protocol](https://datatracker.ietf.org/doc/html/rfc7143)
- [RFC 1350 — TFTP Protocol](https://datatracker.ietf.org/doc/html/rfc1350)
```

- [ ] **Step 2: Commit**

```bash
git add GUIDE.md
git commit -m "docs: Russian-language setup guide for XBoot

Covers Rust installation, compilation, config structure, file
preparation, startup/shutdown, network setup, and troubleshooting.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 9: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace -- --nocapture`
Expected: all tests PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings

- [ ] **Step 3: Run rustfmt**

Run: `cargo fmt -- --check`
Expected: no diff

- [ ] **Step 4: Run fuzz build**

Run: `cargo +nightly fuzz build`
Expected: all fuzz targets build

- [ ] **Step 5: Commit any final fixups or mark done**

If clean:
```bash
echo "Phase 07 complete: all tests pass, clippy clean, fmt clean, fuzz ok"
```
