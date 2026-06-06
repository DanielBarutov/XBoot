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
