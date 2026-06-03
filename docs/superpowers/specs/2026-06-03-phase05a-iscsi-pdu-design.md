# Phase 05a — iSCSI PDU-кодек (словарь провода)

**Дата:** 2026-06-03
**Фаза:** 05a — первая под-фаза Phase 05 (iSCSI Target). См. `docs/superpowers/plans/2026-06-02-xboot-roadmap.md`
**Зависит от:** ничего нового (чистая логика «байты ↔ структуры»)
**Главная спека:** `docs/superpowers/specs/2026-06-02-xboot-diskless-boot-engine-design.md` (§2 «iSCSI Target»)

## Контекст: нарезка Phase 05

iSCSI-таргет — крупнейшая подсистема проекта, поэтому Phase 05 разбита на три под-фазы,
каждая со своей спекой/планом/циклом реализации (как фазы 02–04):

- **05a — PDU-кодек** *(эта спека)*: разбор/сборка байтов провода. Чистая синхронная логика
  в `xboot-core`, без I/O, без состояния, без новых зависимостей. Фаззится и юнит-тестируется
  изолированно.
- **05b — SCSI-команды**: по разобранному CDB + `LUN → Volume` сформировать SCSI-ответ
  (`INQUIRY`, `REPORT LUNS`, `READ CAPACITY 10/16`, `TEST UNIT READY`, `READ/WRITE 10/16`,
  `SYNCHRONIZE CACHE`, минимальный `MODE SENSE`). Чистая логика поверх готового `Volume`.
- **05c — Сессия/транспорт**: конечный автомат сессии (login → full-feature → logout) по схеме
  **sans-I/O** (синхронное ядро: запрос-PDU → ответ-PDU + эффекты на `Volume`), тонкая
  tokio-обёртка TCP поверх, реестр `Target` (LUN → Volume), фейковый инициатор в тестах.
  tokio добавляется в `xboot-core` только здесь.

**Архитектурные решения, согласованные на брейншторме (действуют на всю Phase 05):**
- Транспорт — **sans-I/O ядро + тонкий async-адаптер** (вариант A). Почти весь стек остаётся
  синхронным, детерминированным, тестируемым без сети.
- Размещение — модуль `iscsi/` внутри `xboot-core` (не отдельный крейт), рядом со `storage/`,
  `volume/`, `cache/`.
- **Стандартный подмножество RFC 7143**: байт-в-байт правильный формат провода и login-переговоры,
  чтобы настоящий инициатор (iPXE/Windows) потом заработал без переписывания. Реальная
  Windows-проверка — позже, в E2E-фазе.
- **AuthMethod=None** (доверенный LAN). CHAP — вне области v1.

## Цель (05a)

Дать остальным слоям iSCSI надёжный «словарь провода»: функции, которые превращают входящие
байты в типизированные структуры запросов и типизированные ответы — обратно в байты. Кодек
**не хранит состояние**, **не делает I/O** и **не интерпретирует смысл** (нумерацию проверяет
05c, CDB разбирает 05b). Главный инвариант: на любом мусоре на входе — `Ok` или `Err`,
**никогда panic**.

## 1. Раскладка модуля

Новый модуль `crates/xboot-core/src/iscsi/`, объявляется в `lib.rs` (`pub mod iscsi;`).
Новых зависимостей нет.

```
iscsi/
  mod.rs    // pub use; общие константы (опкоды, BHS_LEN = 48); PduError
  pdu.rs    // Bhs-парсинг, enum Request (decode), response-билдеры (encode)
  text.rs   // key=value сегменты (login / SendTargets): parse_pairs / encode_pairs
```

05b добавит `iscsi/scsi.rs`; 05c — `iscsi/session.rs`, `iscsi/target.rs` и транспорт.
Сегодня создаются только `mod.rs`, `pdu.rs`, `text.rs`.

**Граница ответственности.** 05a знает *формат* байтов, но не *смысл*. Он извлечёт из заголовка
`CmdSN`, `CDB[16]`, текст login-сегмента — но не проверяет валидность последовательностей и не
исполняет команды.

## 2. Формат PDU (наш подмножество)

iSCSI-PDU = **48-байтовый Basic Header Segment (BHS)** + опциональный сегмент данных,
дополненный нулями до кратности 4 байт.

В нашем подмножестве:
- **Дайджесты согласованы в `None`** (HeaderDigest=None, DataDigest=None) → между BHS и данными,
  и после данных, *нет* CRC32C-полей. CRC32C-дайджесты — **вне области** (05c согласует None;
  если инициатор настаивает на CRC32C, это обрабатывается отказом в login’е, не здесь).
- **AHS не используется**: `TotalAHSLength` (байт 4 BHS) обязан быть `0`. Наши CDB укладываются в
  16 байт внутри BHS.

Общая структура BHS (поля, которые кодек обязан извлекать корректно; все многобайтовые —
**big-endian / сетевой порядок**):

| Смещение | Поле | Назначение |
|---|---|---|
| 0 | `I` (bit) + `Opcode` (младшие 6 бит) | тип PDU; `I` = immediate |
| 1 | `F` (bit) + opcode-специфичные флаги | например R/W у SCSI Command |
| 4 | `TotalAHSLength` | обязан быть 0 |
| 5..8 | `DataSegmentLength` (3 байта) | длина сегмента данных в байтах |
| 8..16 | `LUN` / opcode-специфика | 8-байтовый LUN |
| 16..20 | `InitiatorTaskTag` (ITT) | тег задачи |
| 20..48 | opcode-специфичные поля | CmdSN, ExpStatSN, CDB, и т.д. |

## 3. Модель данных

### 3.1. Декодирование: `enum Request`

Опкоды инициатора (RFC 7143 §11), которые парсим:

```rust
pub enum Request {
    Login(LoginRequest),       // 0x03: T,C, CSG,NSG, version_max/min, ISID[6], TSIH,
                               //       ITT, CID, CmdSN, ExpStatSN, data = текст key=value
    Text(TextRequest),         // 0x04: F,C, ITT, TTT, CmdSN, ExpStatSN, data = key=value (SendTargets)
    ScsiCommand(ScsiCommand),  // 0x01: F,R,W, ATTR, LUN, ITT, ExpectedDataTransferLength,
                               //       CmdSN, ExpStatSN, cdb: [u8;16]
    DataOut(ScsiDataOut),      // 0x05: LUN, ITT, TTT, ExpStatSN, DataSN, BufferOffset, F, payload
    NopOut(NopOut),            // 0x00: LUN, ITT, TTT, CmdSN, ExpStatSN, ping_data
    Logout(LogoutRequest),     // 0x06: reason_code, CID, ITT, CmdSN, ExpStatSN
    TaskMgmt(TaskMgmt),        // 0x02: function, LUN, ITT, ReferencedTaskTag, CmdSN, ExpStatSN
    Unsupported { opcode: u8, itt: u32 },  // любой иной опкод
}
```

**Ключевой принцип устойчивости:** неизвестный/неподдержанный опкод — **не** ошибка парсинга, а
вариант `Request::Unsupported { opcode, itt }`. Так 05c сможет ответить `Reject` (или, по RFC, —
проигнорировать там, где это уместно), а не падать. `Err` возвращается только при структурной
порче байтов (см. §5).

Поля каждого варианта — простые типы (`u8`/`u16`/`u32`/`u64`/`[u8;16]`/`Vec<u8>` для payload).
Числа последовательностей (`CmdSN`, `ExpStatSN`, `DataSN`) **переносятся как есть**; их валидация
и продвижение — задача 05c.

### 3.2. Кодирование: response-билдеры

Опкоды таргета, которые сериализуем (каждый — структура с публичными полями и методом
`encode(&self) -> Vec<u8>`):

```rust
LoginResponse   // 0x23: T,C, CSG,NSG, version, ISID, TSIH, StatSN, ExpCmdSN, MaxCmdSN,
                //       status_class/detail, data = текст ответных key=value
TextResponse    // 0x24: F,C, ITT, TTT, StatSN..., data = key=value (ответ SendTargets)
ScsiResponse    // 0x21: response, status (GOOD/CHECK CONDITION), ITT, StatSN, ExpCmdSN,
                //       MaxCmdSN, residual, data = sense-данные (опц.)
ScsiDataIn      // 0x25: F,A,S,O,U, status, LUN, ITT, TTT, StatSN, DataSN, BufferOffset, payload (READ)
R2t             // 0x31: LUN, ITT, TTT, StatSN, R2TSN, BufferOffset, DesiredDataTransferLength
NopIn           // 0x20: LUN, ITT, TTT, StatSN, ExpCmdSN, MaxCmdSN, ping_data
LogoutResponse  // 0x26: response, StatSN, ExpCmdSN, MaxCmdSN
Reject          // 0x3f: reason, StatSN, ExpCmdSN, MaxCmdSN, data = заголовок отвергнутого PDU
```

`encode()` сам вычисляет `DataSegmentLength` и добивает сегмент данных нулями до кратности 4.
Поля нумерации (`StatSN`, `ExpCmdSN`, `MaxCmdSN`) задаёт вызывающий 05c — кодек только пишет
их в нужные байты.

### 3.3. `text.rs` — key=value сегменты

Текстовые сегменты login/Text — это `Key=Value` записи, разделённые нулевым байтом
(`Key=Value\0Key=Value\0…`). Две чистые функции:

```rust
pub fn parse_pairs(data: &[u8]) -> Vec<(String, String)>;   // мусор/неполные пары — пропускаются, не паникуют
pub fn encode_pairs(pairs: &[(String, String)]) -> Vec<u8>; // добавляет \0-терминатор после каждой пары
```

Парсинг login-ключей (`AuthMethod`, `MaxRecvDataSegmentLength`, `SendTargets`, …) и сами
переговоры — задача 05c; 05a только превращает байты ↔ список пар.

## 4. Поток decode / encode

- `decode(buf: &[u8]) -> Result<(Request, usize), PduError>`
  - требует `buf.len() >= 48`, иначе `ShortHeader`;
  - извлекает `Opcode`, флаги, `TotalAHSLength`, `DataSegmentLength`, `ITT` и opcode-поля;
  - `TotalAHSLength != 0` → `UnexpectedAhs`;
  - `DataSegmentLength > MAX_DATA_SEGMENT` (кап, дефолт 256 КиБ) → `DataSegmentTooLong`;
  - требует, чтобы в `buf` хватало `48 + data_len + padding(data_len)` байт, иначе `ShortData`;
  - возвращает `(Request, consumed)`, где `consumed` = полная длина PDU с padding’ом
    (транспорт 05c использует это, чтобы отрезать PDU из накопленного буфера сокета).
- Билдеры: заполнить поля → `encode()` → `Vec<u8>` (BHS + padded data). `DataSegmentLength`
  и padding считаются автоматически.

`padding(n) = (4 - (n % 4)) % 4`.

## 5. Обработка ошибок

```rust
pub enum PduError {
    ShortHeader,        // < 48 байт
    ShortData,          // объявленный сегмент данных не помещается в буфер
    UnexpectedAhs,      // TotalAHSLength != 0 (AHS вне области)
    DataSegmentTooLong, // DataSegmentLength > MAX_DATA_SEGMENT (защита от абсурдных длин)
}
```

(через `thiserror`, как `ConfigError`/прочие в проекте.)

**Инвариант (проверяется фаззером):** `decode` на любом `&[u8]` возвращает `Ok` или `Err` и
**никогда не паникует** — ни `unwrap`/`expect`, ни паник от срезов/индексации, ни переполнений.
Доступ к байтам — через проверенные срезы и `from_be_bytes`, не через прямую индексацию без
проверки длины.

## 6. Стратегия тестирования (TDD)

Тест пишется до реализации (особенно golden-байты — там легко ошибиться в смещениях).

- **Golden-байты (юнит).** Несколько руками собранных реальных PDU в hex:
  - Login-request → ожидаемые `CSG/NSG/T`, `ISID`, `ITT`, `CmdSN`, и распарсенный текст-сегмент;
  - SCSI Command (например, READ(10)) → флаги R/W, `LUN`, `ITT`, `EDTL`, `cdb[0]=0x28`;
  - проверка, что `encode()` ответа даёт ожидаемые байты (ScsiResponse GOOD, R2T, Reject).
- **proptest round-trip.** Для каждого билдера: `build → encode → decode → те же поля`
  (там, где тип имеет симметричный request, иначе — декодируем через тестовый помощник).
  Для текста: `pairs → encode_pairs → parse_pairs → те же пары`.
- **proptest robustness.** `decode(произвольные байты)` → `Ok | Err`, не паникует
  (быстрая «дешёвая» версия фаззинга прямо в юнит-тестах).
- **cargo-fuzz.** Цель `iscsi_pdu`: `decode(data)` на входе фаззера — никогда не паникует.
  Инфраструктура `fuzz/` уже есть в репозитории с фазы 02; добавляется один fuzz-target.
- **Краевые случаи (юнит):** пустой сегмент данных; сегмент длиной не кратной 4 (padding);
  `Unsupported` для неизвестного опкода с сохранением `itt`; `UnexpectedAhs`;
  `DataSegmentTooLong`; ровно-48-байтовый PDU без данных.

## Готово, когда

- `decode` корректно разбирает все перечисленные в §3.1 опкоды, неизвестные → `Unsupported`,
  структурно битые → типизированный `PduError`;
- все билдеры из §3.2 кодируют корректные BHS + padded data; round-trip proptest зелёный;
- golden-байтовые тесты на реальных PDU проходят;
- fuzz-target `iscsi_pdu` не находит паник;
- `cargo test`, `cargo clippy --all-targets` и `cargo fmt --check` чисты.

## Вне области Phase 05a

- Любое состояние сессии, нумерация (валидация `CmdSN`/`StatSN`), login-переговоры — это 05c.
- Интерпретация CDB и SCSI-ответы (`INQUIRY`, `READ CAPACITY`, …) — это 05b.
- CRC32C header/data дайджесты (согласуем `None` в 05c).
- AHS (Additional Header Segments), bidirectional-команды, расширенный CDB > 16 байт.
- SNACK, MC/S (несколько соединений на сессию), ErrorRecoveryLevel > 0.
- TCP/сеть/tokio — добавляются в 05c.
