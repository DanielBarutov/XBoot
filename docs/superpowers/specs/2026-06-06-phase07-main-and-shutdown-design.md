# Phase 07 — main() и Graceful Shutdown — Дизайн

**Дата:** 2026-06-06
**Статус:** утверждён
**Контекст:** XBoot, после Phase 06c (HTTP boot-скрипт сервер)

## 1. Задача

Phase 07 склеивает четыре готовых сервиса (proxyDHCP, TFTP, HTTP, iSCSI) в работающий процесс
с graceful shutdown и документирует полный цикл от установки Rust до загрузки клиента.

Что входит:
1. **`iscsi_port` в `BootConfig`** — конфигурация порта iSCSI (дефолт 3260)
2. **`Arc<RwLock<TargetRegistry>>`** — унифицированный тип для HTTP-писателя и iSCSI-читателя
3. **`CancellationToken` во все сервисы** — graceful shutdown через `tokio::select!`
4. **`main.rs`** — склеить всё в один процесс
5. **`GUIDE.md`** — подробное руководство на русском: установка Rust, компиляция, настройка, запуск

## 2. Архитектура main()

```
                    ┌──────────────────────────────────────────┐
                    │              main.rs                      │
                    │                                           │
  config.toml ──►──┤  load → validate → init                   │
                    │                                           │
                    │  ┌─────────────┐  ┌───────────────────┐  │
                    │  │ ClientManager│  │Arc<RwLock<        │  │
                    │  │ (backing     │  │  TargetRegistry>> │  │
                    │  │  stores)     │  │                   │  │
                    │  └──────┬───────┘  └───────┬───────────┘  │
                    │         │                   │              │
                    │         ▼                   │              │
                    │  ┌──────────────────────────┼──────────┐  │
                    │  │  tokio::select!          │          │  │
                    │  │                          │          │  │
                    │  │  dhcp::serve(cfg, token) │          │  │
                    │  │  tftp::serve(cfg, token) │          │  │
                    │  │  http::serve(cfg, mgr, reg, token)  │  │
                    │  │  iscsi::serve(listener, reg, token) │  │
                    │  │  tokio::signal::ctrl_c() → cancel   │  │
                    │  │                          │          │  │
                    │  └──────────────────────────┴──────────┘  │
                    │                                           │
                    └──────────────────────────────────────────┘
```

**Поток запуска:**

1. Парсим `config.toml` через `config::load_from_path`
2. Валидируем — если ошибка, выходим с ExitCode::FAILURE
3. Строим `ClientManager::new(&cfg)` — открывает все RO backing stores
4. Создаём `Arc<RwLock<TargetRegistry>>` — пустой реестр
5. Биндим `TcpListener` на `boot.bind:boot.iscsi_port` для iSCSI
6. Создаём `CancellationToken`
7. `tokio::select!` на 4 сервиса + `ctrl_c`
8. При выходе любого — процесс завершается

## 3. Компоненты

### 3.1 BootConfig — `iscsi_port`

```toml
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
http_bind       = "0.0.0.0"
http_port       = 80
iscsi_port      = 3260        # новый, дефолт 3260
tftp_root       = "/srv/xboot/tftp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
```

Поле `iscsi_port: u16`, дефолт 3260 (стандартный порт iSCSI). Сервер iSCSI слушает на `boot.bind:boot.iscsi_port`. Валидация: `iscsi_port != 0`.

### 3.2 TargetRegistry — `Arc<RwLock<>>`

Унифицируем тип: везде `Arc<RwLock<TargetRegistry>>`.

| Файл | Было | Стало |
|------|------|-------|
| `net/http/mod.rs:serve()` | `Arc<Mutex<TargetRegistry>>` | `Arc<RwLock<TargetRegistry>>` |
| `net/http/mod.rs:handle()` | `Arc<Mutex<TargetRegistry>>` | `Arc<RwLock<TargetRegistry>>` |
| `net/http/mod.rs:start_server()` (тест) | `Arc<Mutex<...>>` | `Arc<RwLock<...>>` |
| `iscsi/transport.rs:serve()` | `Arc<TargetRegistry>` | `Arc<RwLock<TargetRegistry>>` |
| `iscsi/transport.rs:handle_conn()` | `Arc<TargetRegistry>` | `Arc<RwLock<TargetRegistry>>` |
| `iscsi/session/mod.rs:Connection::new()` | `Arc<TargetRegistry>` | `Arc<RwLock<TargetRegistry>>` |

- `http::handle` пишет: `registry.write().await.insert(iqn, target)`
- `transport` читает при login: `registry.read().await.get(iqn)`
- `Connection` хранит `Arc<RwLock<TargetRegistry>>` внутри

### 3.3 CancellationToken во все сервисы

Каждый сервис получает `CancellationToken` и завершается при его отмене.

**proxyDHCP и TFTP (UDP, циклы по пакетам):**

```rust
pub async fn serve(cfg: BootConfig, token: CancellationToken) -> std::io::Result<()> {
    // ... bind socket ...
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
                tracing::info!("dhcp: shutting down");
                return Ok(());
            }
        }
    }
}
```

Аналогично для TFTP — select на каждом `recv_from`.

**HTTP (TcpListener accept loop):**

```rust
pub async fn serve(
    cfg: Arc<Config>,
    manager: Arc<ClientManager>,
    registry: Arc<RwLock<TargetRegistry>>,
    token: CancellationToken,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _peer) = result?;
                let cfg = cfg.clone();
                let manager = manager.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    // ... handle connection ...
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

**iSCSI transport (TcpListener accept loop) — аналогично HTTP.**

### 3.4 main.rs

```rust
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

    let cfg = match xboot_core::config::load_from_path(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // Init ClientManager (opens backing stores)
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

    let boot = cfg.boot.as_ref().expect("[boot] section required");
    let iscsi_addr = SocketAddr::new(boot.bind, boot.iscsi_port);

    let dhcp = tokio::spawn({
        let boot_cfg = cfg.boot.clone().unwrap();
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::dhcp::server::serve(boot_cfg, t).await {
                tracing::error!("dhcp: {e}");
            }
        }
    });

    let tftp = tokio::spawn({
        let boot_cfg = cfg.boot.clone().unwrap();
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::tftp::server::serve(boot_cfg, t).await {
                tracing::error!("tftp: {e}");
            }
        }
    });

    let http = tokio::spawn({
        let c = cfg.clone();
        let m = manager.clone();
        let r = registry.clone();
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::http::serve(c, m, r, t).await {
                tracing::error!("http: {e}");
            }
        }
    });

    let iscsi = tokio::spawn({
        let r = registry.clone();
        let t = token.clone();
        async move {
            let listener = match TcpListener::bind(iscsi_addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("iscsi: bind {}: {e}", iscsi_addr);
                    return;
                }
            };
            if let Err(e) = xboot_core::iscsi::transport::serve(listener, r, t).await {
                tracing::error!("iscsi: {e}");
            }
        }
    });

    let ctrl_c = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("SIGINT received, shutting down...");
        token.cancel();
    });

    // Wait for all services to exit.
    tokio::select! {
        _ = dhcp => {}
        _ = tftp => {}
        _ = http => {}
        _ = iscsi => {}
        _ = ctrl_c => {}
    }

    ExitCode::SUCCESS
}
```

### 3.5 GUIDE.md — руководство на русском

Структура:

1. **Установка окружения** — установка Rust через rustup, проверка `rustc --version`
2. **Клонирование и компиляция** — `git clone`, `cargo build --release`
3. **Структура конфига** — полный пример `config.toml` с комментариями на русском
4. **Подготовка файлов** — куда положить бинарники iPXE, размещение образов VHD/VHDX
5. **Запуск** — `xboot config.toml`, вывод логов, как проверить что всё работает
6. **Настройка сети** — proxyDHCP рядом с существующим DHCP, настройка iPXE
7. **Диагностика** — типовые ошибки и их решение

## 4. Файловая карта

| Файл | Действие | Ответственность |
|------|----------|-----------------|
| `crates/xboot-core/src/config/model.rs` | Modify | Добавить `iscsi_port: u16` в `BootConfig` |
| `crates/xboot-core/src/config/validate.rs` | Modify | Валидация `iscsi_port != 0` |
| `crates/xboot-core/src/net/http/mod.rs` | Modify | `Mutex` → `RwLock`, `CancellationToken` |
| `crates/xboot-core/src/net/dhcp/server.rs` | Modify | Добавить `CancellationToken` параметр |
| `crates/xboot-core/src/net/tftp/server.rs` | Modify | Добавить `CancellationToken` параметр |
| `crates/xboot-core/src/iscsi/transport.rs` | Modify | `Arc<TReg>` → `Arc<RwLock<TReg>>`, `CancellationToken` |
| `crates/xboot-core/src/iscsi/session/mod.rs` | Modify | `Arc<TReg>` → `Arc<RwLock<TReg>>` |
| `crates/xboot/src/main.rs` | Modify | Полная реализация main() |
| `crates/xboot/Cargo.toml` | Modify | Добавить `tokio-util` для `CancellationToken` |
| `crates/xboot-core/Cargo.toml` | Modify | Добавить `tokio-util` |
| `GUIDE.md` | Create | Руководство на русском языке |

## 5. Тестирование

| Слой | Что | Как |
|------|-----|-----|
| Unit | Валидация `iscsi_port` | Новый тест `rejects_zero_iscsi_port` |
| Unit | `client_iqn` + `parse_mac` | Уже покрыто в 06c |
| Integration | HTTP handler с `RwLock` | Обновить сигнатуры в тестах, перезапустить |
| Integration | iSCSI transport с `RwLock` | Обновить сигнатуры в тестах, перезапустить |
| Smoke | Компиляция `xboot` бинарника | `cargo build -p xboot` |
| Smoke | CLI тесты | Существующие 3 теста, обновить при необходимости |

E2E-тест заменён на подробное русское руководство (`GUIDE.md`).

## 6. Состояние после Phase 07

После этого этапа XBoot — работающий бинарник:

- Парсит конфиг TOML
- Открывает backing stores
- Слушает 4 порта: :67 DHCP, :69 TFTP, :80 HTTP, :3260 iSCSI
- Принимает PXE-запрос → отдаёт iPXE → отдаёт boot-скрипт → регистрирует iSCSI target
- Windows-клиент может подключиться по iSCSI и загрузиться
- Ctrl+C → graceful shutdown всех сервисов
- Подробное руководство на русском описывает полный цикл настройки
