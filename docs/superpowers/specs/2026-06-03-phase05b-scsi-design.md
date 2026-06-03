# Phase 05b — SCSI-команды (CDB → ответ)

**Дата:** 2026-06-03
**Фаза:** 05b — вторая под-фаза Phase 05 (iSCSI Target).
**Зависит от:** 05a (PDU-кодек: `ScsiCommand`, билдеры ответов) и Phase 03 (`Volume`).
**Главная спека:** `docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md` (§2 «iSCSI Target»)
**Брейншторм:** 2026-06-03 (см. ниже зафиксированные решения)

## Контекст: место 05b в Phase 05

05a даёт разобранный `ScsiCommand { cdb: [u8;16], lun, read, write, edtl, data }`
и билдеры ответных PDU. Phase 03 даёт `Volume` (master + COW overlay) с синхронными
`size_bytes()` / `read_at(offset, buf)` / `write_at(offset, buf)` / `reset()`.

**05b — мост между ними:** по CDB и номеру LUN выполнить SCSI-команду над нужным `Volume`
и вернуть результат (данные + статус, либо `CHECK CONDITION` со sense). 05b **не знает**
про провод, номера последовательности (StatSN/CmdSN), нарезку Data-In и сборку Data-Out —
это забота 05c.

## Решения брейншторма (2026-06-03)

- **Граница 05b/05c — чистая функция + готовые данные.** 05b = `target.execute(&cmd, write_data)
  -> ScsiOutcome { status, data, sense }`. Получает уже собранный буфер записи, возвращает
  все данные чтения в памяти. Вся нарезка Data-In, выдача R2T и сборка Data-Out — в 05c.
  Никакого I/O, никакого состояния сессии — sans-I/O, как и весь стек Phase 05.
- **Набор команд — спека + экстра для Windows.** Базовый набор спеки плюс команды, которые
  реально шлёт ядро Windows при iSCSI-загрузке (REQUEST SENSE, VPD-страницы INQUIRY,
  PREVENT/ALLOW MEDIUM REMOVAL, START STOP UNIT), чтобы на E2E не упереться в CHECK CONDITION
  на первом же шаге.
- **Логический размер блока — 512 B по умолчанию, поле в `LogicalUnit`.** Самый совместимый
  вариант для Windows-загрузки; параметр оставляет дорогу к 4Kn для игровых томов без
  переписывания логики. Логический размер блока **не зависит** от внутреннего размера COW-блока.
- **Организация — `ScsiTarget`, владеющий картой LUN.** Вся логика LUN (резолв, REPORT LUNS,
  невалидный LUN) в одном месте; 05c просто зовёт `execute` и сериализует результат.

## Архитектура: модули

Новый подмодуль `crates/xboot-core/src/iscsi/scsi/` (рядом с PDU-кодеком 05a, т.к. работает
с `ScsiCommand` из `iscsi::pdu`):

| Файл | Ответственность |
|------|-----------------|
| `mod.rs` | `ScsiTarget`, `LogicalUnit`, `ScsiOutcome`; вход `execute` + резолв LUN + диспатч по `cdb[0]` |
| `cdb.rs` | разбор полей CDB (opcode, LBA, transfer length для 10/16-байтовых форм) + обработчики команд |
| `sense.rs` | константы статусов и sense-ключей + билдер fixed-format sense |

## Типы данных

```rust
pub struct ScsiOutcome {
    pub status: u8,      // GOOD (0x00) | CHECK CONDITION (0x02)
    pub data: Vec<u8>,   // data-in для инициатора (READ, INQUIRY, ...); пусто если нет
    pub sense: Vec<u8>,  // fixed-format sense; пусто когда status == GOOD
}

pub struct LogicalUnit {
    volume: Volume,         // Phase 03: master + COW overlay
    block_size: u32,        // по умолчанию 512
    vendor: [u8; 8],        // "XBOOT   "
    product: [u8; 16],      // напр. "VDISK"
    revision: [u8; 4],      // "0001"
    serial: String,         // VPD page 0x80
    removable: bool,        // обычно false
}

pub struct ScsiTarget { luns: Vec<Option<LogicalUnit>> }  // индекс = номер LUN

impl ScsiTarget {
    pub fn new(luns: Vec<Option<LogicalUnit>>) -> Self;
    pub fn execute(&self, cmd: &ScsiCommand, write_data: &[u8]) -> ScsiOutcome;
}
```

**Резолв LUN.** iSCSI-поле LUN — 8 байт (`cmd.lun: u64`). Для плоской адресации (наш случай —
единицы LUN) номер = `(lun >> 48) & 0xff` (второй байт структуры LUN при peripheral-адресации,
когда старший байт = 0). Неизвестный метод адресации, номер вне диапазона `luns`, или `None`
в слоте → `CHECK CONDITION` (LOGICAL UNIT NOT SUPPORTED).

**Residual** (underflow/overflow) считает 05c, сравнивая `data.len()` с `cmd.edtl`; 05b про EDTL
не знает.

**Владение.** `ScsiTarget` владеет своими `LogicalUnit` (один target на клиента — согласуется
со спекой). `Volume` здесь не клонируется и не оборачивается для Sync; конкурентный доступ к
одному target — забота 05c.

## Набор команд и поведение

Диспатч по `cdb[0]` (после успешного резолва LUN):

| CDB | Команда | Поведение |
|-----|---------|-----------|
| `0x00` | TEST UNIT READY | GOOD |
| `0x12` | INQUIRY | EVPD=0 → стандартные 36 байт; EVPD=1 → VPD-страница по page code |
| `0x03` | REQUEST SENSE | fixed-format «NO SENSE» (autosense ездит в SCSI Response; состояние не храним) |
| `0x1A` / `0x5A` | MODE SENSE(6/10) | минимальный заголовок параметров без block descriptor; на caching page (0x08) — короткая страница с WCE=1 |
| `0x25` | READ CAPACITY(10) | last LBA = `blocks-1` (если > 0xFFFFFFFF → 0xFFFFFFFF), block length |
| `0x9E` (sa `0x10`) | READ CAPACITY(16) | 32-байтовый ответ: 8-байт last LBA + block length |
| `0x28` / `0x88` | READ(10/16) | прочитать `len` блоков из `Volume` в `data` |
| `0x2A` / `0x8A` | WRITE(10/16) | записать `write_data` в `Volume` (в overlay) |
| `0x35` / `0x91` | SYNCHRONIZE CACHE(10/16) | GOOD (writeback в RAM/volatile — no-op) |
| `0xA0` | REPORT LUNS | список сконфигурированных LUN в report-luns-формате |
| `0x1E` | PREVENT/ALLOW MEDIUM REMOVAL | GOOD (no-op) |
| `0x1B` | START STOP UNIT | GOOD (no-op) |
| прочее | — | CHECK CONDITION, ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE (`0x20`) |

**Стандартный INQUIRY (EVPD=0), 36 байт:** peripheral qualifier 000b + device type `0x00`
(direct-access block device); RMB по `removable`; версия `0x05` (SPC-3); response data format
`0x02`; additional length 31; vendor/product/revision из `LogicalUnit`.

**VPD-страницы (EVPD=1):** `0x00` — список поддерживаемых [0x00, 0x80, 0x83]; `0x80` — серийник
(ASCII); `0x83` — один T10-дескриптор (vendor + product + serial) для стабильной идентичности
диска в Windows. Неизвестный page code → CHECK CONDITION (INVALID FIELD IN CDB).

**Адресная математика READ/WRITE:** `offset = lba * block_size`, `len = transfer_length * block_size`.
Предпроверка диапазона: `total_blocks = size_bytes / block_size`; если
`lba + transfer_length > total_blocks` → CHECK CONDITION (LBA OUT OF RANGE) **до** обращения к
`Volume`, чтобы выдать корректный sense, а не сырой io::Error. `transfer_length == 0` для
READ(10)/WRITE(10) означает «0 блоков» → GOOD без данных. Для WRITE буфер `write_data` должен
иметь длину `len`; несоответствие → CHECK CONDITION (INVALID FIELD IN CDB).

## Обработка ошибок и sense

Один билдер в `sense.rs`, fixed-format (response code `0x70`), 18 байт:

```rust
pub fn fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8>;

pub const GOOD: u8 = 0x00;
pub const CHECK_CONDITION: u8 = 0x02;

// sense keys
// NO_SENSE=0x00, NOT_READY=0x02, MEDIUM_ERROR=0x03, ILLEGAL_REQUEST=0x05, UNIT_ATTENTION=0x06
```

Используемые комбинации (key / ASC / ASCQ):

| Ситуация | Sense |
|----------|-------|
| Неизвестный opcode | ILLEGAL_REQUEST / `0x20` `0x00` — INVALID COMMAND OPERATION CODE |
| Неизвестный/нелегальный LUN | ILLEGAL_REQUEST / `0x25` `0x00` — LOGICAL UNIT NOT SUPPORTED |
| LBA вне диапазона | ILLEGAL_REQUEST / `0x21` `0x00` — LBA OUT OF RANGE |
| Неизвестное поле в CDB (VPD page, длина буфера) | ILLEGAL_REQUEST / `0x24` `0x00` — INVALID FIELD IN CDB |
| io::Error от `Volume` на чтении (после предпроверок) | MEDIUM_ERROR / `0x11` `0x00` — UNRECOVERED READ ERROR |
| io::Error от `Volume` на записи (после предпроверок) | MEDIUM_ERROR / `0x0C` `0x00` — WRITE ERROR |

**Инварианты (проверяются тестами):**
- `status == CHECK_CONDITION` ⟺ `sense` непустой; `status == GOOD` ⟺ `sense` пустой.
- **Никогда не паникуем.** Любой кривой CDB даёт корректный CHECK CONDITION. Срез CDB всегда
  `[u8;16]` (из 05a), так что чтение полей не выходит за границы.

io::Error от `Volume` после предпроверок диапазона практически невозможен, но ловится и
превращается в MEDIUM_ERROR, а не разворачивается в панику.

## Тестирование (TDD — норма проекта)

**Unit-тесты (тест до реализации, по команде):**
- INQUIRY: тип устройства `0x00`, версия, additional length, vendor/product; VPD 0x00/0x80/0x83 формат.
- READ CAPACITY(10/16): last LBA = blocks−1, block length = 512; overflow > 4 ГиБ → 0xFFFFFFFF в (10).
- READ: байты совпадают с тем, что отдаёт `Volume` (из master и из overlay после записи).
- WRITE→READ round-trip: запись уходит в overlay, master не меняется.
- `transfer_length == 0` → GOOD, пустые данные.
- Неизвестный opcode / неизвестный LUN / LBA вне диапазона → CHECK CONDITION с правильным sense.
- REPORT LUNS: перечислены ровно сконфигурированные LUN.
- TEST UNIT READY / SYNC CACHE / PREVENT-ALLOW / START-STOP → GOOD.

**Property-тесты (`proptest`):**
- Произвольный 16-байтовый CDB + произвольный LUN → `execute` никогда не паникует.
- Инвариант: `status == CHECK_CONDITION` ⟺ `sense` непустой.
- Для READ в пределах диапазона: `data.len() == transfer_length * block_size`.

**Fuzz:** отдельный таргет не обязателен в 05b (CDB фикс. длины `[u8;16]`, а proptest
«never panics» уже покрывает вход). Отмечено как возможное расширение, вне объёма 05b.

**Покрытие:** 05b — один из критичных модулей (наряду с COW/cache/parser), целимся в высокий
процент через `tarpaulin`.

**E2E:** реальный Windows-инициатор отложен на финальную фазу. Проверка 05b — через
unit/proptest; интеграция с фейковым инициатором появится в 05c.

## Границы (что НЕ входит в 05b)

- Провод, PDU-сборка ответов, StatSN/CmdSN/ExpCmdSN — 05c.
- Нарезка данных чтения на Data-In PDU, выдача R2T, сборка Data-Out по R2T — 05c.
- Login-переговоры, конечный автомат сессии, TCP, tokio — 05c.
- Конфиг (TOML) построения `ScsiTarget`/`LogicalUnit` из дисков клиента — Disk/Client Manager
  (отдельная фаза); 05b принимает уже собранные `LogicalUnit`.
