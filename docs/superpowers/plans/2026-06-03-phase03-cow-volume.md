# Фаза 03 — COW Volume Manager — План реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Собрать `Volume` — виртуальный диск клиента из read-only мастера (`BackingStore`, Фаза 02) и volatile COW-оверлея, с побайтово корректными `read_at`/`write_at` и `reset`.

**Architecture:** `Volume` владеет `Box<dyn BackingStore>` (мастер, RO) и `Box<dyn OverlayStore>` (оверлей). Чтение: оверлей → мастер. Запись: всегда в оверлей блоками по 4 KiB; частичная запись делает read-modify-write. Оверлей спрятан за трейтом; в этой фазе единственная реализация — `RamOverlay` на `RwLock<HashMap>`. Всё на `&self` (внутренняя мутабельность), поэтому `Volume: Send + Sync` и шарится как `Arc<Volume>`.

**Tech Stack:** Rust (edition 2021), std only (`std::sync::RwLock`, `std::collections::HashMap`). Dev: `proptest` (инвариант COW против плоской байтовой модели), уже в dev-deps.

**Spec / roadmap:** `docs/superpowers/specs/2026-06-03-phase03-cow-volume-design.md` и `docs/superpowers/plans/2026-06-02-xboot-roadmap.md` (Фаза 03).

> **Замечание по языку:** doc-комментарии в коде — на английском (консистентность с `storage/`). Проза плана — на русском.

---

## Структура файлов фазы

| Файл | Ответственность |
|---|---|
| `crates/xboot-core/src/volume/mod.rs` | `Volume` + публичный API (`new`, `size_bytes`, `read_at`, `write_at`, `reset`), константа `BLOCK` (ре-экспорт), проверка границ, разбивка по блокам, RMW. Тесты read/write/reset/изоляции/конкурентности/proptest. |
| `crates/xboot-core/src/volume/overlay.rs` | Трейт `OverlayStore` + `RamOverlay` (`RwLock<HashMap<u64, Box<[u8]>>>`) + константа `BLOCK`. Юнит-тесты `RamOverlay`. |
| `crates/xboot-core/src/lib.rs` | Регистрация `pub mod volume;`. |

`Volume` платформо-независим и не знает про iSCSI — это «виртуальный диск с COW», который Фаза 05 дёргает командами READ/WRITE.

---

## Справка: как работает COW здесь (прочти один раз)

`BLOCK = 4096`. Диск делится на блоки по 4 KiB. Последний («хвостовой») блок короче, если
`size_bytes` не кратен 4096; его фактическая длина — `min(BLOCK, size_bytes - blk*BLOCK)`.

- **READ блока:** если блок есть в оверлее — отдать из оверлея; иначе прочитать из мастера.
- **WRITE блока, покрытого полностью:** сохранить срез прямо в оверлей.
- **WRITE блока, покрытого частично:** read-modify-write — взять текущее содержимое блока
  (из оверлея, если он там есть; иначе из мастера), наложить записываемые байты, сохранить
  весь блок в оверлей.

Мастер никогда не пишется: у `BackingStore` просто нет write-метода.

---

## Task 1: Модуль `volume` + трейт `OverlayStore` + `RamOverlay`

**Files:**
- Create: `crates/xboot-core/src/volume/overlay.rs`
- Create: `crates/xboot-core/src/volume/mod.rs`
- Modify: `crates/xboot-core/src/lib.rs`

- [ ] **Step 1: Написать падающие тесты для `RamOverlay`**

Создать `crates/xboot-core/src/volume/overlay.rs`:

```rust
use std::collections::HashMap;
use std::sync::RwLock;

/// COW block size in bytes. The overlay stores data in blocks of this size
/// (the disk's tail block may be shorter).
pub const BLOCK: u64 = 4096;

/// Per-client store of overwritten blocks. Access is copy-based to avoid
/// lock-lifetime borrow issues. Volatile: `reset` discards everything.
pub trait OverlayStore: Send + Sync {
    /// If block `blk` exists, fill `buf` with its data and return true.
    /// `buf.len()` must equal the stored block's length.
    fn read_block(&self, blk: u64, buf: &mut [u8]) -> bool;
    /// Store/overwrite block `blk` with exactly `data`.
    fn write_block(&self, blk: u64, data: &[u8]);
    /// Discard all overlay contents (volatile "reboot").
    fn reset(&self);
}

/// RAM implementation of `OverlayStore` backed by a `HashMap` under an `RwLock`.
#[derive(Default)]
pub struct RamOverlay {
    blocks: RwLock<HashMap<u64, Box<[u8]>>>,
}

impl RamOverlay {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miss_returns_false_and_leaves_buf() {
        let ov = RamOverlay::new();
        let mut buf = [9u8; 4];
        assert!(!ov.read_block(0, &mut buf));
        assert_eq!(buf, [9, 9, 9, 9]);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let ov = RamOverlay::new();
        ov.write_block(7, &[1, 2, 3, 4]);
        let mut buf = [0u8; 4];
        assert!(ov.read_block(7, &mut buf));
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn write_overwrites_existing() {
        let ov = RamOverlay::new();
        ov.write_block(7, &[1, 2, 3, 4]);
        ov.write_block(7, &[5, 6, 7, 8]);
        let mut buf = [0u8; 4];
        assert!(ov.read_block(7, &mut buf));
        assert_eq!(buf, [5, 6, 7, 8]);
    }

    #[test]
    fn reset_clears_all() {
        let ov = RamOverlay::new();
        ov.write_block(0, &[1; 4]);
        ov.write_block(1, &[2; 4]);
        ov.reset();
        let mut buf = [0u8; 4];
        assert!(!ov.read_block(0, &mut buf));
        assert!(!ov.read_block(1, &mut buf));
    }
}
```

Создать `crates/xboot-core/src/volume/mod.rs` (пока только подключение оверлея):

```rust
mod overlay;

pub use overlay::{OverlayStore, RamOverlay, BLOCK};
```

Зарегистрировать модуль — в `crates/xboot-core/src/lib.rs` добавить строку `pub mod volume;` (после `pub mod storage;`), чтобы получилось:

```rust
pub mod config;
pub mod net;
pub mod storage;
pub mod volume;
```

- [ ] **Step 2: Запустить тесты — убедиться, что не компилируется/падает**

Run: `cargo test -p xboot-core volume::overlay -- --nocapture`
Expected: ошибка компиляции — методы трейта не реализованы для `RamOverlay` (`OverlayStore` не impl).

- [ ] **Step 3: Реализовать `OverlayStore` для `RamOverlay`**

Добавить в `crates/xboot-core/src/volume/overlay.rs` перед `#[cfg(test)]`:

```rust
impl OverlayStore for RamOverlay {
    fn read_block(&self, blk: u64, buf: &mut [u8]) -> bool {
        let map = self.blocks.read().unwrap();
        match map.get(&blk) {
            Some(data) => {
                buf.copy_from_slice(data);
                true
            }
            None => false,
        }
    }

    fn write_block(&self, blk: u64, data: &[u8]) {
        self.blocks
            .write()
            .unwrap()
            .insert(blk, data.to_vec().into_boxed_slice());
    }

    fn reset(&self) {
        self.blocks.write().unwrap().clear();
    }
}
```

- [ ] **Step 4: Запустить тесты — должны пройти**

Run: `cargo test -p xboot-core volume::overlay`
Expected: PASS (4 теста).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/volume/overlay.rs crates/xboot-core/src/volume/mod.rs crates/xboot-core/src/lib.rs
git commit -m "feat(volume): OverlayStore trait + RamOverlay"
```

---

## Task 2: Скелет `Volume` — `new`, `size_bytes`, проверка границ

**Files:**
- Modify: `crates/xboot-core/src/volume/mod.rs`

- [ ] **Step 1: Написать падающий тест на `size_bytes` + тестовый мастер**

Заменить содержимое `crates/xboot-core/src/volume/mod.rs` на:

```rust
use std::io;

use crate::storage::BackingStore;

mod overlay;

pub use overlay::{OverlayStore, RamOverlay, BLOCK};

/// A client's virtual disk: a read-only master plus a volatile COW overlay.
/// Reads consult the overlay then the master. Writes always land in the
/// overlay; the master is never modified.
pub struct Volume {
    master: Box<dyn BackingStore>,
    overlay: Box<dyn OverlayStore>,
}

impl Volume {
    pub fn new(master: Box<dyn BackingStore>, overlay: Box<dyn OverlayStore>) -> Self {
        Self { master, overlay }
    }

    /// Virtual size of the volume = the master's size (the overlay never grows it).
    pub fn size_bytes(&self) -> u64 {
        self.master.size_bytes()
    }

    /// Actual length of block `blk`, accounting for a short tail block.
    /// Caller must ensure `blk * BLOCK < size_bytes`.
    fn block_len(&self, blk: u64) -> usize {
        let start = blk * BLOCK;
        std::cmp::min(BLOCK, self.size_bytes() - start) as usize
    }

    /// Reject ranges that overflow or run past the end of the volume.
    fn check_bounds(&self, offset: u64, len: usize) -> io::Result<()> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "range overflows u64"))?;
        if end > self.size_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("out of range: {end} > {}", self.size_bytes()),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory backing store for tests: the bytes ARE the disk.
    struct Mem(Vec<u8>);

    impl BackingStore for Mem {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let s = offset as usize;
            buf.copy_from_slice(&self.0[s..s + buf.len()]);
            Ok(())
        }
    }

    fn vol(master_bytes: Vec<u8>) -> Volume {
        Volume::new(Box::new(Mem(master_bytes)), Box::new(RamOverlay::new()))
    }

    #[test]
    fn size_is_master_size() {
        let v = vol(vec![0u8; 10_000]);
        assert_eq!(v.size_bytes(), 10_000);
    }
}
```

- [ ] **Step 2: Запустить тест**

Run: `cargo test -p xboot-core volume::tests::size_is_master_size`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/volume/mod.rs
git commit -m "feat(volume): Volume skeleton (new, size_bytes, bounds)"
```

---

## Task 3: `read_at` (оверлей → мастер)

**Files:**
- Modify: `crates/xboot-core/src/volume/mod.rs`

- [ ] **Step 1: Написать падающие тесты на чтение**

Добавить в `mod tests` (после `size_is_master_size`):

```rust
    // Master = 0,1,2,...,255,0,1,... over 10000 bytes.
    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 256) as u8).collect()
    }

    #[test]
    fn read_miss_returns_master() {
        let v = vol(ramp(10_000));
        let mut buf = [0u8; 5];
        v.read_at(100, &mut buf).unwrap();
        assert_eq!(buf, [100, 101, 102, 103, 104]);
    }

    #[test]
    fn read_hit_returns_overlay() {
        let v = vol(ramp(10_000));
        // Overwrite block 0 entirely with 0xAB.
        v.overlay.write_block(0, &[0xAB; BLOCK as usize]);
        let mut buf = [0u8; 4];
        v.read_at(10, &mut buf).unwrap();
        assert_eq!(buf, [0xAB; 4]);
    }

    #[test]
    fn read_spans_overlay_and_master() {
        let v = vol(ramp(10_000));
        // Overlay covers block 0 only; read crosses into block 1 (master).
        v.overlay.write_block(0, &[0xAB; BLOCK as usize]);
        let mut buf = [0u8; 8];
        v.read_at(BLOCK - 4, &mut buf).unwrap();
        // first 4 bytes from overlay (0xAB), next 4 from master at offset BLOCK..
        let mut expect = [0xABu8; 8];
        for (i, e) in expect[4..].iter_mut().enumerate() {
            *e = ((BLOCK as usize + i) % 256) as u8;
        }
        assert_eq!(buf, expect);
    }

    #[test]
    fn read_tail_block_shorter_than_block() {
        // size = 1 block + 100 bytes; read the final 100-byte tail.
        let n = BLOCK as usize + 100;
        let v = vol(ramp(n));
        let mut buf = [0u8; 100];
        v.read_at(BLOCK, &mut buf).unwrap();
        let expect: Vec<u8> = (0..100).map(|i| ((BLOCK as usize + i) % 256) as u8).collect();
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn read_zero_len_is_ok() {
        let v = vol(ramp(10));
        let mut buf: [u8; 0] = [];
        v.read_at(10, &mut buf).unwrap(); // at EOF, len 0 -> ok
    }

    #[test]
    fn read_out_of_range_errors() {
        let v = vol(ramp(10));
        let mut buf = [0u8; 5];
        assert!(v.read_at(8, &mut buf).is_err());
    }
```

- [ ] **Step 2: Запустить — убедиться, что падает**

Run: `cargo test -p xboot-core volume::tests::read_`
Expected: ошибка компиляции — метода `read_at` нет.

- [ ] **Step 3: Реализовать `read_at`**

Добавить в `impl Volume` (после `check_bounds`):

```rust
    /// Read exactly `buf.len()` bytes starting at byte `offset`, consulting the
    /// overlay first and falling back to the master, block by block.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check_bounds(offset, buf.len())?;
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let blk = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let blen = self.block_len(blk);
            let take = std::cmp::min(blen - within, buf.len() - done);
            let dst = &mut buf[done..done + take];

            let mut block = vec![0u8; blen];
            if self.overlay.read_block(blk, &mut block) {
                dst.copy_from_slice(&block[within..within + take]);
            } else {
                self.master.read_at(blk * BLOCK + within as u64, dst)?;
            }
            pos += take as u64;
            done += take;
        }
        Ok(())
    }
```

- [ ] **Step 4: Запустить тесты — должны пройти**

Run: `cargo test -p xboot-core volume::tests::read_`
Expected: PASS (6 тестов).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/volume/mod.rs
git commit -m "feat(volume): read_at with overlay-then-master COW reads"
```

---

## Task 4: `write_at` (в оверлей, с read-modify-write)

**Files:**
- Modify: `crates/xboot-core/src/volume/mod.rs`

- [ ] **Step 1: Написать падающие тесты на запись**

Добавить в `mod tests`:

```rust
    #[test]
    fn write_full_block_then_read() {
        let v = vol(ramp(10_000));
        let data = [0xCD; BLOCK as usize];
        v.write_at(0, &data).unwrap();
        let mut buf = [0u8; BLOCK as usize];
        v.read_at(0, &mut buf).unwrap();
        assert_eq!(buf.to_vec(), data.to_vec());
    }

    #[test]
    fn write_partial_new_block_does_rmw() {
        // Write 4 bytes in the middle of block 0 (not yet in overlay).
        // The rest of the block must still read back as the master's bytes.
        let v = vol(ramp(10_000));
        v.write_at(10, &[0xEE; 4]).unwrap();
        let mut buf = [0u8; 20];
        v.read_at(0, &mut buf).unwrap();
        let mut expect: Vec<u8> = (0..20u8).collect();
        expect[10..14].copy_from_slice(&[0xEE; 4]);
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn write_partial_existing_block_patches() {
        let v = vol(ramp(10_000));
        v.write_at(0, &[0x11; 8]).unwrap(); // block 0 now in overlay (partial)
        v.write_at(2, &[0x22; 2]).unwrap(); // patch existing overlay block
        let mut buf = [0u8; 8];
        v.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0x11, 0x11, 0x22, 0x22, 0x11, 0x11, 0x11, 0x11]);
    }

    #[test]
    fn write_spans_two_blocks() {
        let v = vol(ramp(10_000));
        // 8-byte write straddling the block 0/1 boundary.
        v.write_at(BLOCK - 4, &[0x33; 8]).unwrap();
        let mut buf = [0u8; 8];
        v.read_at(BLOCK - 4, &mut buf).unwrap();
        assert_eq!(buf, [0x33; 8]);
        // bytes just before and after are untouched master bytes
        let mut before = [0u8; 1];
        v.read_at(BLOCK - 5, &mut before).unwrap();
        assert_eq!(before[0], ((BLOCK as usize - 5) % 256) as u8);
    }

    #[test]
    fn write_tail_block() {
        let n = BLOCK as usize + 100;
        let v = vol(ramp(n));
        v.write_at(BLOCK + 10, &[0x44; 5]).unwrap();
        let mut buf = [0u8; 100];
        v.read_at(BLOCK, &mut buf).unwrap();
        let mut expect: Vec<u8> = (0..100).map(|i| ((BLOCK as usize + i) % 256) as u8).collect();
        expect[10..15].copy_from_slice(&[0x44; 5]);
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn write_zero_len_is_ok() {
        let v = vol(ramp(10));
        v.write_at(10, &[]).unwrap();
    }

    #[test]
    fn write_out_of_range_errors() {
        let v = vol(ramp(10));
        assert!(v.write_at(8, &[0u8; 5]).is_err());
    }

    #[test]
    fn write_never_touches_master_bytes_outside_overlay() {
        // After a partial write, reading an untouched later block returns master.
        let v = vol(ramp(10_000));
        v.write_at(0, &[0x55; 4]).unwrap();
        let mut buf = [0u8; 4];
        v.read_at(5000, &mut buf).unwrap();
        let expect: Vec<u8> = (5000..5004).map(|i| (i % 256) as u8).collect();
        assert_eq!(buf.to_vec(), expect);
    }
```

- [ ] **Step 2: Запустить — убедиться, что падает**

Run: `cargo test -p xboot-core volume::tests::write_`
Expected: ошибка компиляции — метода `write_at` нет.

- [ ] **Step 3: Реализовать `write_at`**

Добавить в `impl Volume` (после `read_at`):

```rust
    /// Write `buf` starting at byte `offset` into the overlay, block by block.
    /// A block fully covered by the range is stored directly; a partially
    /// covered block is read-modified-written (from the overlay if present,
    /// else from the master) so untouched bytes are preserved.
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.check_bounds(offset, buf.len())?;
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let blk = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let blen = self.block_len(blk);
            let take = std::cmp::min(blen - within, buf.len() - done);
            let src = &buf[done..done + take];

            if take == blen {
                self.overlay.write_block(blk, src);
            } else {
                let mut block = vec![0u8; blen];
                if !self.overlay.read_block(blk, &mut block) {
                    self.master.read_at(blk * BLOCK, &mut block)?;
                }
                block[within..within + take].copy_from_slice(src);
                self.overlay.write_block(blk, &block);
            }
            pos += take as u64;
            done += take;
        }
        Ok(())
    }
```

- [ ] **Step 4: Запустить тесты — должны пройти**

Run: `cargo test -p xboot-core volume::tests::write_`
Expected: PASS (8 тестов).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/volume/mod.rs
git commit -m "feat(volume): write_at into overlay with read-modify-write"
```

---

## Task 5: `reset` + жизненный цикл + изоляция

**Files:**
- Modify: `crates/xboot-core/src/volume/mod.rs`

- [ ] **Step 1: Написать падающие тесты на reset/lifecycle/изоляцию**

Добавить в `mod tests`:

```rust
    use std::sync::Arc;

    /// Wrapper to share one master between several Volumes via Arc.
    struct ArcStore(Arc<dyn BackingStore>);
    impl BackingStore for ArcStore {
        fn size_bytes(&self) -> u64 {
            self.0.size_bytes()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            self.0.read_at(offset, buf)
        }
    }

    #[test]
    fn reset_discards_writes_and_restores_master() {
        let v = vol(ramp(10_000));
        v.write_at(0, &[0x77; 16]).unwrap();
        v.reset();
        let mut buf = [0u8; 16];
        v.read_at(0, &mut buf).unwrap();
        let expect: Vec<u8> = (0..16u8).collect();
        assert_eq!(buf.to_vec(), expect);
    }

    #[test]
    fn two_volumes_share_master_but_isolate_writes() {
        let master: Arc<dyn BackingStore> = Arc::new(Mem(ramp(10_000)));
        let a = Volume::new(Box::new(ArcStore(master.clone())), Box::new(RamOverlay::new()));
        let b = Volume::new(Box::new(ArcStore(master.clone())), Box::new(RamOverlay::new()));

        a.write_at(0, &[0x99; 8]).unwrap();

        // b never sees a's write.
        let mut buf = [0u8; 8];
        b.read_at(0, &mut buf).unwrap();
        assert_eq!(buf.to_vec(), (0..8u8).collect::<Vec<_>>());

        // a does see its own write.
        a.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0x99; 8]);
    }
```

- [ ] **Step 2: Запустить — убедиться, что падает**

Run: `cargo test -p xboot-core volume::tests::reset_ volume::tests::two_volumes`
Expected: ошибка компиляции — метода `reset` нет.

- [ ] **Step 3: Реализовать `reset`**

Добавить в `impl Volume` (после `write_at`):

```rust
    /// Discard the client's overlay (volatile reboot); reads return the master.
    pub fn reset(&self) {
        self.overlay.reset();
    }
```

- [ ] **Step 4: Запустить тесты — должны пройти**

Run: `cargo test -p xboot-core volume::tests`
Expected: PASS (все тесты модуля, включая 2 новых).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/volume/mod.rs
git commit -m "feat(volume): reset + lifecycle and isolation tests"
```

---

## Task 6: proptest — инвариант COW против плоской байтовой модели

**Files:**
- Modify: `crates/xboot-core/src/volume/mod.rs`

Эталонная модель: плоский `Vec<u8>`, инициализированный содержимым мастера. Те же
операции применяются к `Volume` и к модели; после случайной последовательности read/write
их содержимое обязано совпадать байт-в-байт. Это математически фиксирует корректность COW.

- [ ] **Step 1: Написать proptest**

Добавить в конец `crates/xboot-core/src/volume/mod.rs` отдельный модуль:

```rust
#[cfg(test)]
mod prop {
    use super::tests_support::Mem;
    use super::*;
    use proptest::prelude::*;

    /// One operation against both the Volume and the reference byte array.
    #[derive(Debug, Clone)]
    enum Op {
        Read { offset: usize, len: usize },
        Write { offset: usize, bytes: Vec<u8> },
    }

    const SIZE: usize = 3 * BLOCK as usize + 123; // a few blocks + a short tail

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..SIZE, 0..(2 * BLOCK as usize)).prop_map(|(offset, len)| {
                let len = len.min(SIZE - offset);
                Op::Read { offset, len }
            }),
            (0..SIZE, prop::collection::vec(any::<u8>(), 0..(2 * BLOCK as usize))).prop_map(
                |(offset, mut bytes)| {
                    bytes.truncate(SIZE - offset);
                    Op::Write { offset, bytes }
                }
            ),
        ]
    }

    proptest! {
        #[test]
        fn volume_matches_flat_model(ops in prop::collection::vec(op_strategy(), 0..200)) {
            // Master is a fixed ramp; model starts equal to it.
            let master: Vec<u8> = (0..SIZE).map(|i| (i % 256) as u8).collect();
            let mut model = master.clone();
            let v = Volume::new(Box::new(Mem(master)), Box::new(RamOverlay::new()));

            for op in ops {
                match op {
                    Op::Read { offset, len } => {
                        let mut got = vec![0u8; len];
                        v.read_at(offset as u64, &mut got).unwrap();
                        prop_assert_eq!(&got, &model[offset..offset + len]);
                    }
                    Op::Write { offset, bytes } => {
                        v.write_at(offset as u64, &bytes).unwrap();
                        model[offset..offset + bytes.len()].copy_from_slice(&bytes);
                    }
                }
            }

            // Final full read must equal the model.
            let mut full = vec![0u8; SIZE];
            v.read_at(0, &mut full).unwrap();
            prop_assert_eq!(full, model);
        }
    }
}
```

- [ ] **Step 2: Вынести тестовый `Mem` в общий под-модуль**

`prop` использует `Mem` из `tests`. Чтобы оба модуля его видели, вынести `Mem` в общий
`#[cfg(test)]` под-модуль. В `crates/xboot-core/src/volume/mod.rs`:

1. Удалить определение `struct Mem(...)` и его `impl BackingStore` из `mod tests`.
2. Добавить рядом с `mod tests` новый модуль и импорт внутри `tests`:

```rust
#[cfg(test)]
mod tests_support {
    use super::*;

    /// In-memory backing store for tests: the bytes ARE the disk.
    pub struct Mem(pub Vec<u8>);

    impl BackingStore for Mem {
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let s = offset as usize;
            buf.copy_from_slice(&self.0[s..s + buf.len()]);
            Ok(())
        }
    }
}
```

3. В начале `mod tests` добавить `use super::tests_support::Mem;` (а `vol(...)` уже
   ссылается на `Mem` — теперь через импорт).

- [ ] **Step 3: Запустить proptest**

Run: `cargo test -p xboot-core volume::prop`
Expected: PASS (инвариант держится на всех сгенерированных последовательностях).

- [ ] **Step 4: Запустить весь модуль — ничего не сломалось**

Run: `cargo test -p xboot-core volume`
Expected: PASS (все юнит-тесты + proptest).

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/src/volume/mod.rs
git commit -m "test(volume): proptest COW invariant vs flat byte model"
```

---

## Task 7: Конкурентный смоук (`Arc<Volume>`)

**Files:**
- Modify: `crates/xboot-core/src/volume/mod.rs`

Цель — подтвердить, что `Volume` действительно `Send + Sync` и параллельные read/write через
`Arc<Volume>` не паникуют и не портят данные. Полноценная нагрузка — Фаза 05.

- [ ] **Step 1: Написать конкурентный тест**

Добавить в `mod tests`:

```rust
    #[test]
    fn concurrent_writers_isolated_by_block() {
        use std::thread;

        // Each thread owns one full block and writes its id into it.
        let v: Arc<Volume> = Arc::new(vol(ramp(8 * BLOCK as usize)));
        let mut handles = Vec::new();
        for t in 0u8..8 {
            let v = v.clone();
            handles.push(thread::spawn(move || {
                let data = vec![t; BLOCK as usize];
                v.write_at(t as u64 * BLOCK, &data).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // Every block must contain exactly its writer's id.
        for t in 0u8..8 {
            let mut buf = vec![0u8; BLOCK as usize];
            v.read_at(t as u64 * BLOCK, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == t), "block {t} corrupted");
        }
    }
```

- [ ] **Step 2: Запустить тест**

Run: `cargo test -p xboot-core volume::tests::concurrent_writers_isolated_by_block`
Expected: PASS (компилируется только если `Volume: Send + Sync`).

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/volume/mod.rs
git commit -m "test(volume): concurrent Arc<Volume> smoke test"
```

---

## Task 8: Финальная проверка — fmt / clippy / весь набор

**Files:** нет правок кода, кроме исправлений, если линтеры что-то найдут.

- [ ] **Step 1: Форматирование**

Run: `cargo fmt --all`
Then: `cargo fmt --all --check`
Expected: без вывода (чисто).

- [ ] **Step 2: Clippy строго**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: без предупреждений.
Готча из Фазы 02: clippy на текущем stable флагует `manual_is_multiple_of` — если такое
всплывёт, заменить `x % n == 0` на `x.is_multiple_of(n)`. (В коде этой фазы прямых сравнений
остатка с нулём нет; `pos % BLOCK` используется как значение, не как проверка.)

- [ ] **Step 3: Весь тестовый набор**

Run: `cargo test --all`
Expected: PASS — прежние тесты (59 unit + 3 interop) плюс новые тесты `volume`.

- [ ] **Step 4: Commit (если линтеры что-то поправили)**

```bash
git add -A
git commit -m "chore(volume): clippy/fmt cleanup for phase 03"
```

Если правок не было — пропустить коммит.

---

## Done, когда

- `Volume::read_at` отдаёт оверлей → мастер; `write_at` всегда пишет в оверлей (RMW для
  частичных блоков); мастер доказуемо неизменен (у `BackingStore` нет write-метода).
- `reset` сбрасывает volatile-оверлей; после него чтения возвращают чистый мастер.
- proptest-инвариант COW (Volume == плоская байтовая модель) зелёный.
- Изоляция между клиентами и конкурентный смоук зелёные.
- `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --all` — зелёные.
