# Phase 06c — HTTP Boot Script Server — Design

**Дата:** 2026-06-06
**Статус:** утверждён
**Контекст:** XBoot, после Phase 06b (TFTP)

## 1. Задача

После того как iPXE загружен через proxyDHCP→TFTP, он делает HTTP GET на `http://<server>/boot.ipxe?mac=<mac>` и ожидает получить iPXE-скрипт, который говорит ему, какой iSCSI-таргет подключить и с какого LUN грузиться.

Phase 06c реализует:
1. **HTTP-сервер** на hyper 1.x — принимает GET, отдаёт boot-скрипт
2. **ClientManager** — резолвит MAC → клиент + диски из конфига TOML
3. **BootScript-генератор** — генерирует текст iPXE-скрипта
4. **Регистрацию ScsiTarget** в TargetRegistry — клиентский таргет готов к iSCSI-подключению

## 2. Архитектура

```
iPXE ──GET /boot.ipxe?mac=..──►  HTTP Server (hyper 1.x)
                                       │
                                       ▼
                                  ClientManager
                                  (MAC → Client + диски)
                                       │
                                       ▼
                                  BootScript generator
                                  (iPXE скрипт с sanboot)
                                       │
                                       ▼
                                  TargetRegistry
                                  (регистрация ScsiTarget)
```

**Файловая карта:**

| Файл | Действие | Ответственность |
|------|----------|-----------------|
| `crates/xboot-core/src/config/model.rs` | Modify | Добавить `http_bind: IpAddr`, `http_port: u16` в `BootConfig` |
| `crates/xboot-core/src/config/validate.rs` | Modify | Валидация http_port (не 0) |
| `crates/xboot-core/src/manager/mod.rs` | Create | `ClientManager` — MAC→Client резолвинг |
| `crates/xboot-core/src/net/http/mod.rs` | Create | HTTP-сервер на hyper 1.x |
| `crates/xboot-core/src/net/http/boot_script.rs` | Create | Генератор iPXE-скрипта |
| `crates/xboot-core/src/net/mod.rs` | Modify | `pub mod http` |
| `crates/xboot-core/src/lib.rs` | Modify | `pub mod manager` |
| `crates/xboot-core/Cargo.toml` | Modify | Добавить hyper, tokio `rt-multi-thread` |

## 3. Компоненты

### 3.1 BootConfig — новые поля

```toml
[boot]
server_ip       = "192.168.1.10"
bind            = "0.0.0.0"
http_bind       = "0.0.0.0"     # IP для HTTP-сервера
http_port       = 80            # порт HTTP-сервера
tftp_root       = "/srv/xboot/tftp"
bios_filename   = "undionly.kpxe"
uefi_filename   = "ipxe.efi"
http_script_url = "http://192.168.1.10/boot.ipxe"
```

`http_bind` и `http_port` управляют, на каком адресе и порту слушает HTTP-сервер. Валидация: `http_port` не ноль.

### 3.2 ClientManager

```rust
pub struct ClientManager {
    clients: HashMap<String, Arc<ClientConfig>>,  // ключ — MAC в нижнем регистре
    defaults: Option<Arc<ClientConfig>>,
    // Кэш открытых RO backing store'ов (по id диска)
    stores: HashMap<String, Arc<dyn BackingStore>>,
}

pub struct ClientConfig {
    pub mac: String,
    pub name: Option<String>,
    pub system_disk_id: String,     // ссылка на Disk.id
    pub game_disk_ids: Vec<String>,
    pub writeback_disk_id: String,
}

impl ClientManager {
    /// Построить из Config:
    /// - проиндексировать клиентов по MAC (lowercase)
    /// - закэшировать client_defaults
    /// - открыть все RO backing-store'ы (image + game диски)
    pub fn new(cfg: &Config) -> Result<Self, BuildError>;

    /// MAC → ClientConfig. Неизвестный → defaults. None = клиентов нет.
    pub fn resolve(&self, mac: &str) -> Option<&ClientConfig>;

    /// Получить открытый RO backing-store по ID диска.
    pub fn get_store(&self, disk_id: &str) -> Option<Arc<dyn BackingStore>>;
}
```

- Дисковые ссылки уже провалидированы `config::validate()` — `ClientManager` не дублирует проверки.
- `BuildError` — на случай, если `client_defaults` ссылается на несуществующий диск (крайний случай после валидации).
- RO-бэкинги открываются один раз при старте и хранятся в `stores: HashMap<String, Arc<dyn BackingStore>>`.
- Writeback-путь хранится как `PathBuf` — свежий `RamOverlay` создаётся при каждой регистрации таргета.
- В v1 (volatile) оверлей чисто в памяти — `backing` writeback-диска не используется (задел на будущее persistent writeback).

### 3.3 BootScript generator

```rust
/// Генерирует iPXE-скрипт для загрузки Windows с iSCSI-таргета.
/// IQN формат: iqn.2026-06.dev.xboot:<name> или iqn.2026-06.dev.xboot:<mac>
pub fn generate(iqn: &str, server_ip: &str) -> String;
```

Пример выхлопа:

```
#!ipxe
set initiator-iqn iqn.2026-06.dev.xboot:pc-01
sanboot iscsi:192.168.1.10::::iqn.2026-06.dev.xboot:pc-01
```

`sanboot` сам обнаружит LUN через `REPORT LUNS`.

### 3.4 IQN конвенция

- Если у клиента задан `name` → `iqn.2026-06.dev.xboot:<name>`
- Если `name` не задан → `iqn.2026-06.dev.xboot:<mac>` (дефисы, lowercase)
- Это детерминировано и не требует новых полей в конфиге.

### 3.5 HTTP Server (hyper 1.x)

```
GET  /boot.ipxe?mac=aa:bb:cc:dd:ee:01  → 200 text/plain (iPXE скрипт)
GET  /boot.ipxe                         → 400 Missing 'mac' parameter
GET  /anything-else                     → 404 Not Found
POST /boot.ipxe                         → 405 Method Not Allowed
```

Сервер принимает `Arc<Config>`, `Arc<ClientManager>`, `Arc<TargetRegistry>`.

**Поток обработки одного запроса:**

1. Принять TCP-соединение (hyper)
2. Распарсить HTTP-запрос
3. Если не `GET /boot.ipxe?mac=...` → 404/405
4. Извлечь `mac` из query-параметров
5. `ClientManager::resolve(mac)` → `ClientConfig`
6. Если не найден → 404
7. Собрать `ScsiTarget`:
   - System LUN: Volume c RO-бэкингом из кэша + свежий RamOverlay
   - Game LUN-ы: аналогично
   - Writeback: отдельный Volume для каждого game LUN
8. Вычислить IQN по конвенции (name или mac)
9. `TargetRegistry::insert(iqn, scsi_target)`
10. `BootScript::generate(iqn, server_ip)` → тело ответа
11. 200 OK, `Content-Type: text/plain`

## 4. Тестирование

| Слой | Что | Как |
|------|-----|-----|
| Unit | `ClientManager` | MAC→Client, fallback на defaults, ошибка при битых ссылках |
| Unit | `BootScript::generate()` | Снапшот выхлопа скрипта |
| Integration | HTTP server happy path | `reqwest` (dev), поднять сервер на `localhost:0`, GET `/boot.ipxe?mac=...`, проверить ответ |
| Integration | HTTP server errors | 404 неизвестный путь, 400 без mac, 404 неизвестный MAC, 405 не-GET |
| Integration | Регистрация в TargetRegistry | После GET таргет виден в `TargetRegistry::get(iqn)` |
| Fuzz | `BootScript::generate()` | Не паникует на любом входном IQN |

## 5. Обработка ошибок

- Невалидный HTTP → 400
- MAC не найден + нет `client_defaults` → 404
- Ошибка открытия backing store → 500 + лог
- Ошибка регистрации в TargetRegistry → 500 + лог
- Паника в обработчике → hyper отдаёт 500, сервер продолжает работать

## 6. Состояние после Phase 06c

После этого этапа у нас полный cold-boot pipeline:
- proxyDHCP (06a) — отвечает на PXE-запрос
- TFTP (06b) — отдаёт iPXE-бинарник
- HTTP (06c) — отдаёт boot-скрипт и регистрирует iSCSI-таргет

Остаётся Phase 07 — склеить всё в `main()`, добавить graceful shutdown, и первый живой E2E-тест на Windows-клиенте.
