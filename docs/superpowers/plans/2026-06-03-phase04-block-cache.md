# Phase 04 — Block Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a shared, byte-budgeted LRU RAM cache of read-only master blocks (`CachedStore`) that decorates any `BackingStore`, so hot master blocks are held in RAM once for all clients of a disk.

**Architecture:** `CachedStore` wraps a `Box<dyn BackingStore>` (the real VHD/VHDX/raw master) plus a `Mutex<LruState>` holding an `lru::LruCache<u64, Box<[u8]>>` and a manual `current_bytes` counter. Reads go block-by-block (`BLOCK = 4096`, reused from `volume`): a cache hit copies the sub-range under the lock; a miss releases the lock, reads the full block from the master, then re-locks to insert it and evict LRU blocks until `current_bytes <= budget`. The master is read-only, so nothing is ever written back to it. One `Arc<CachedStore>` per master disk is shared across all of that disk's `Volume`s via the existing `ArcStore` wrapper pattern.

**Tech Stack:** Rust 2021, `lru` crate (cache structure), `std::sync::{Mutex, atomic}` (concurrency), `proptest` (transparency + budget invariant), existing `crate::storage::BackingStore` and `crate::volume::BLOCK`.

---

## Reference: design spec

Authoritative design: `docs/superpowers/specs/2026-06-03-phase04-block-cache-design.md`. Key contracts:

- **Read path (§2):** per block — `lock → get`; hit → copy sub-range, unlock; miss → unlock, `inner.read_at(blk*BLOCK, full)` reads the **full block** (length = `block_len`), re-lock, insert, unlock, copy sub-range.
- **Budget (§3):** `LruCache::unbounded()`; track bytes manually. After insert: `while current_bytes > budget { pop_lru } `. Insert a block **only if** `block_len <= budget` (else read-through, no insert). Invariant: `current_bytes <= budget` always.
- **Concurrency (§4):** one `Mutex<LruState>`; lock **not held** during disk I/O; duplicate concurrent misses are benign.
- **Stats (§5):** `hits`/`misses` as `AtomicU64` outside the lock; `blocks`/`bytes` read under the lock.
- **Errors (§6):** propagate `inner.read_at` `Err` unchanged, cache nothing; trust `BackingStore::read_at` bounds contract (no re-validation).

## File Structure

- **Create** `crates/xboot-core/src/cache/mod.rs` — the whole subsystem: `CachedStore`, `CacheStats`, private `LruState`, and all tests. One file is right here: the component is ~130 lines and its pieces change together (mirrors `volume/overlay.rs` sizing).
- **Modify** `crates/xboot-core/src/lib.rs` — register `pub mod cache;`.
- **Modify** `crates/xboot-core/Cargo.toml` — add the `lru` dependency.

`Volume` is **not modified**: it already accepts `master: Box<dyn BackingStore>`, and `CachedStore` *is* a `BackingStore`.

---

### Task 1: Module scaffold, `lru` dependency, and read-through (miss-only) path

Stand up the type and a working read path that always reads straight from the master (counts misses, no caching yet). This locks in the block-walking loop and the `BackingStore` impl.

**Files:**
- Modify: `crates/xboot-core/Cargo.toml`
- Modify: `crates/xboot-core/src/lib.rs`
- Create: `crates/xboot-core/src/cache/mod.rs`

- [ ] **Step 1: Add the `lru` dependency**

In `crates/xboot-core/Cargo.toml`, under `[dependencies]`, add the `lru` line so the section reads:

```toml
[dependencies]
serde.workspace = true
toml.workspace = true
thiserror.workspace = true
lru = "0.12"
```

- [ ] **Step 2: Register the module**

In `crates/xboot-core/src/lib.rs`, add `pub mod cache;` so the file reads:

```rust
pub mod cache;
pub mod config;
pub mod net;
pub mod storage;
pub mod volume;
```

- [ ] **Step 3: Write the failing tests (scaffold + miss path)**

Create `crates/xboot-core/src/cache/mod.rs` with the module doc, imports, the public types, and a test module. The implementation in this step does **no caching** — every block is a miss served from the master. (Expect a `dead_code` warning for the `budget` field until Task 2 uses it; that is fine — warnings do not fail `cargo test`.)

```rust
//! Shared RAM block cache for read-only master images.
//!
//! `CachedStore` decorates a `BackingStore` with an LRU cache of `BLOCK`-sized
//! master blocks under a byte budget. One `Arc<CachedStore>` per master disk is
//! shared across all clients of that disk, so a hot block is held in RAM once
//! for all of them. The master is read-only, so nothing is ever written back.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use lru::LruCache;

use crate::storage::BackingStore;
use crate::volume::BLOCK;

/// Cache contents plus byte accounting, guarded together by one `Mutex`.
struct LruState {
    cache: LruCache<u64, Box<[u8]>>,
    current_bytes: u64,
}

/// Snapshot of cache counters for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub blocks: usize,
    pub bytes: u64,
}

/// A `BackingStore` decorator caching master blocks in RAM (LRU, byte-budgeted).
pub struct CachedStore {
    inner: Box<dyn BackingStore>,
    budget: u64,
    cache: Mutex<LruState>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl CachedStore {
    /// Wrap `inner`, caching its blocks up to `budget_bytes` total.
    pub fn new(inner: Box<dyn BackingStore>, budget_bytes: u64) -> Self {
        Self {
            inner,
            budget: budget_bytes,
            cache: Mutex::new(LruState {
                cache: LruCache::unbounded(),
                current_bytes: 0,
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Hit/miss counters plus the current cached block count and byte total.
    pub fn stats(&self) -> CacheStats {
        let state = self.cache.lock().unwrap();
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            blocks: state.cache.len(),
            bytes: state.current_bytes,
        }
    }
}

impl BackingStore for CachedStore {
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        // No caching yet: every block is a miss served straight from the master.
        let size = self.inner.size_bytes();
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let blk = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let blen = std::cmp::min(BLOCK, size - blk * BLOCK) as usize;
            let take = std::cmp::min(blen - within, buf.len() - done);
            let dst = &mut buf[done..done + take];

            self.misses.fetch_add(1, Ordering::Relaxed);
            self.inner.read_at(pos, dst)?;

            pos += take as u64;
            done += take;
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

    /// Master = 0,1,2,...,255,0,1,... over `n` bytes.
    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 256) as u8).collect()
    }

    fn cached(n: usize, budget: u64) -> CachedStore {
        CachedStore::new(Box::new(Mem(ramp(n))), budget)
    }

    #[test]
    fn size_passes_through_to_master() {
        let c = cached(10_000, 1 << 20);
        assert_eq!(c.size_bytes(), 10_000);
    }

    #[test]
    fn cold_read_returns_master_bytes_and_counts_miss() {
        let c = cached(10_000, 1 << 20);
        let mut buf = [0u8; 5];
        c.read_at(100, &mut buf).unwrap();
        assert_eq!(buf, [100, 101, 102, 103, 104]);
        assert_eq!(c.stats().misses, 1); // single block touched
    }

    #[test]
    fn read_is_byte_correct_across_boundaries_and_tail() {
        // 3 full blocks + a short tail.
        let n = 3 * BLOCK as usize + 123;
        let master = ramp(n);
        let c = CachedStore::new(Box::new(Mem(master.clone())), 1 << 20);
        // Offsets/lengths that cross block boundaries and land in the tail.
        let cases = [
            (0usize, 10usize),
            (BLOCK as usize - 4, 8),
            (BLOCK as usize - 1, 2 * BLOCK as usize + 5),
            (3 * BLOCK as usize, 123),
            (3 * BLOCK as usize + 100, 23),
        ];
        for (off, len) in cases {
            let mut got = vec![0u8; len];
            c.read_at(off as u64, &mut got).unwrap();
            assert_eq!(got, master[off..off + len], "off={off} len={len}");
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p xboot-core cache::`
Expected on first run: compile/run succeeds and tests PASS — because this step's implementation already serves correct master bytes and counts misses. (TDD note: these tests pin the read-walk and `BackingStore` impl; the hit/budget behavior they do **not** yet exercise arrives in Task 2. If anything here fails, fix `read_at` before moving on.)

- [ ] **Step 5: Commit**

```bash
git add crates/xboot-core/Cargo.toml crates/xboot-core/src/lib.rs crates/xboot-core/src/cache/mod.rs
git commit -m "feat(cache): CachedStore scaffold with read-through path"
```

---

### Task 2: Caching, byte budget, and LRU eviction

Replace the miss-only `read_at` with the real cached path, and add `LruState::insert` with budget-bounded LRU eviction. After this task the cache actually caches.

**Files:**
- Modify: `crates/xboot-core/src/cache/mod.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `#[cfg(test)] mod tests` block in `crates/xboot-core/src/cache/mod.rs` (after the Task 1 tests). They exercise hit caching, read-through on a too-small budget, the budget invariant under overflow, and LRU ordering.

```rust
    #[test]
    fn second_read_of_same_block_is_a_hit() {
        let c = cached(10_000, 1 << 20);
        let mut buf = [0u8; 5];
        c.read_at(100, &mut buf).unwrap();
        c.read_at(100, &mut buf).unwrap();
        assert_eq!(buf, [100, 101, 102, 103, 104]);
        let s = c.stats();
        assert_eq!(s.misses, 1);
        assert_eq!(s.hits, 1);
        assert_eq!(s.blocks, 1);
        assert_eq!(s.bytes, BLOCK); // one full block cached
    }

    #[test]
    fn zero_budget_reads_through_without_caching() {
        let c = cached(2 * BLOCK as usize, 0);
        let mut buf = [0u8; 16];
        c.read_at(0, &mut buf).unwrap();
        c.read_at(0, &mut buf).unwrap();
        let s = c.stats();
        assert_eq!(s.blocks, 0); // nothing cached
        assert_eq!(s.bytes, 0);
        assert_eq!(s.misses, 2); // both reads went to the master
        assert_eq!(s.hits, 0);
        // Data is still correct.
        assert_eq!(buf.to_vec(), ramp(16));
    }

    #[test]
    fn budget_smaller_than_block_reads_through() {
        // Full block (4096) never fits a sub-block budget, so it is never cached.
        let c = cached(2 * BLOCK as usize, BLOCK - 1);
        let mut buf = vec![0u8; BLOCK as usize];
        c.read_at(0, &mut buf).unwrap();
        let s = c.stats();
        assert_eq!(s.blocks, 0);
        assert_eq!(s.bytes, 0);
        assert_eq!(buf, ramp(BLOCK as usize));
    }

    #[test]
    fn byte_budget_is_never_exceeded() {
        // 10 full blocks, budget = 3 blocks. Reading all of them never
        // overshoots the budget after any step.
        let c = cached(10 * BLOCK as usize, 3 * BLOCK);
        for blk in 0..10u64 {
            let mut buf = vec![0u8; BLOCK as usize];
            c.read_at(blk * BLOCK, &mut buf).unwrap();
            let s = c.stats();
            assert!(s.bytes <= 3 * BLOCK, "bytes {} over budget", s.bytes);
            assert!(s.blocks <= 3, "blocks {} over budget", s.blocks);
        }
        let s = c.stats();
        assert_eq!(s.blocks, 3);
        assert_eq!(s.bytes, 3 * BLOCK);
    }

    #[test]
    fn lru_keeps_recently_used_block() {
        // Budget = 3 blocks. Read 0,1,2 (fills cache). Touch 0 (now MRU).
        // Read 3 -> evicts the LRU, which must be block 1, not block 0.
        let c = cached(8 * BLOCK as usize, 3 * BLOCK);
        let mut buf = vec![0u8; BLOCK as usize];
        for blk in 0..3u64 {
            c.read_at(blk * BLOCK, &mut buf).unwrap(); // miss x3
        }
        c.read_at(0, &mut buf).unwrap(); // hit, refreshes block 0
        let before = c.stats();
        assert_eq!(before.misses, 3);
        assert_eq!(before.hits, 1);

        c.read_at(3 * BLOCK, &mut buf).unwrap(); // miss, evicts block 1

        // Block 0 is still cached -> hit.
        c.read_at(0, &mut buf).unwrap();
        let after_zero = c.stats();
        assert_eq!(after_zero.hits, 2, "block 0 should have survived eviction");

        // Block 1 was evicted -> miss.
        c.read_at(BLOCK, &mut buf).unwrap();
        let after_one = c.stats();
        assert_eq!(after_one.misses, 5, "block 1 should have been evicted");

        assert!(after_one.bytes <= 3 * BLOCK);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p xboot-core cache::`
Expected: the new tests FAIL — e.g. `second_read_of_same_block_is_a_hit` fails because `hits == 0` (the miss-only path never caches). The Task 1 tests still pass.

- [ ] **Step 3: Add `LruState::insert` (budget-bounded eviction)**

In `crates/xboot-core/src/cache/mod.rs`, add an `impl LruState` block immediately after the `struct LruState { ... }` definition:

```rust
impl LruState {
    /// Insert `data` for block `blk`, update byte accounting, then evict the
    /// least-recently-used blocks until `current_bytes <= budget`.
    ///
    /// Callers must only insert blocks whose length is `<= budget`; that
    /// guarantees the eviction loop terminates leaving the just-inserted block.
    fn insert(&mut self, blk: u64, data: Box<[u8]>, budget: u64) {
        let added = data.len() as u64;
        if let Some(old) = self.cache.put(blk, data) {
            self.current_bytes -= old.len() as u64;
        }
        self.current_bytes += added;
        while self.current_bytes > budget {
            match self.cache.pop_lru() {
                Some((_, v)) => self.current_bytes -= v.len() as u64,
                None => break,
            }
        }
    }
}
```

- [ ] **Step 4: Replace `read_at` with the cached path**

In `crates/xboot-core/src/cache/mod.rs`, replace the entire `fn read_at` body inside `impl BackingStore for CachedStore` with the caching version:

```rust
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let size = self.inner.size_bytes();
        let mut pos = offset;
        let mut done = 0usize;
        while done < buf.len() {
            let blk = pos / BLOCK;
            let within = (pos % BLOCK) as usize;
            let blen = std::cmp::min(BLOCK, size - blk * BLOCK) as usize;
            let take = std::cmp::min(blen - within, buf.len() - done);
            let dst = &mut buf[done..done + take];

            // Try the cache; hold the lock only for the lookup and copy.
            let mut hit = false;
            {
                let mut state = self.cache.lock().unwrap();
                if let Some(block) = state.cache.get(&blk) {
                    dst.copy_from_slice(&block[within..within + take]);
                    hit = true;
                }
            }

            if hit {
                self.hits.fetch_add(1, Ordering::Relaxed);
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                // Lock released during disk I/O: read the FULL block.
                let mut full = vec![0u8; blen];
                self.inner.read_at(blk * BLOCK, &mut full)?;
                dst.copy_from_slice(&full[within..within + take]);
                // Cache it only if a whole block fits the budget; otherwise
                // this is a read-through (data already returned above).
                if blen as u64 <= self.budget {
                    let mut state = self.cache.lock().unwrap();
                    state.insert(blk, full.into_boxed_slice(), self.budget);
                }
            }

            pos += take as u64;
            done += take;
        }
        Ok(())
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p xboot-core cache::`
Expected: PASS — all Task 1 and Task 2 tests green.

- [ ] **Step 6: Commit**

```bash
git add crates/xboot-core/src/cache/mod.rs
git commit -m "feat(cache): LRU caching with byte budget and eviction"
```

---

### Task 3: Property test — transparency + budget invariant

A randomized read sequence must return exactly what a direct `BackingStore` returns, and the byte budget must never be exceeded at any point.

**Files:**
- Modify: `crates/xboot-core/src/cache/mod.rs`

- [ ] **Step 1: Write the failing property test**

Append a new `#[cfg(test)] mod prop` block at the end of `crates/xboot-core/src/cache/mod.rs`:

```rust
#[cfg(test)]
mod prop {
    use super::*;
    use proptest::collection as pcoll;
    use proptest::prelude::*;

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

    const SIZE: usize = 12 * BLOCK as usize + 77; // several blocks + short tail
    const BUDGET: u64 = 5 * BLOCK; // forces eviction under load

    fn read_strategy() -> impl Strategy<Value = (usize, usize)> {
        (0..SIZE, 0..(2 * BLOCK as usize)).prop_map(|(offset, len)| {
            let len = len.min(SIZE - offset);
            (offset, len)
        })
    }

    proptest! {
        #[test]
        fn cache_is_transparent_and_respects_budget(
            reads in pcoll::vec(read_strategy(), 0..300)
        ) {
            let master: Vec<u8> = (0..SIZE).map(|i| (i % 256) as u8).collect();
            let c = CachedStore::new(Box::new(Mem(master.clone())), BUDGET);

            for (offset, len) in reads {
                let mut got = vec![0u8; len];
                c.read_at(offset as u64, &mut got).unwrap();
                prop_assert_eq!(&got, &master[offset..offset + len]);
                prop_assert!(c.stats().bytes <= BUDGET);
            }
        }
    }
}
```

- [ ] **Step 2: Run the property test**

Run: `cargo test -p xboot-core cache::prop`
Expected: PASS (the cache is already correct from Task 2; this test guards against regressions). If it fails, the shrunk counterexample points at the offending read sequence — debug `read_at`/`insert` before continuing.

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/cache/mod.rs
git commit -m "test(cache): proptest transparency and byte-budget invariant"
```

---

### Task 4: Concurrency + `Volume`-over-`CachedStore` integration

Prove the cache is safe under concurrent readers sharing one `Arc<CachedStore>`, and that an unmodified `Volume` reads correctly through the cache.

**Files:**
- Modify: `crates/xboot-core/src/cache/mod.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `#[cfg(test)] mod tests` block in `crates/xboot-core/src/cache/mod.rs` (after the Task 2 tests). They need `Arc`, `thread`, and the `Volume`/`RamOverlay` types.

```rust
    use crate::volume::{RamOverlay, Volume};
    use std::sync::Arc;
    use std::thread;

    /// Share one `Arc<CachedStore>` as a `Box<dyn BackingStore>` for `Volume`.
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
    fn concurrent_readers_get_correct_data_within_budget() {
        // 8 full blocks, budget = 4 blocks, 8 threads each read the whole disk.
        let n = 8 * BLOCK as usize;
        let c: Arc<CachedStore> = Arc::new(cached(n, 4 * BLOCK));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = c.clone();
            handles.push(thread::spawn(move || {
                let mut buf = vec![0u8; n];
                c.read_at(0, &mut buf).unwrap();
                assert_eq!(buf, ramp(n));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Budget invariant holds after concurrent load.
        assert!(c.stats().bytes <= 4 * BLOCK);
    }

    #[test]
    fn volume_reads_through_cache_unmodified() {
        let n = 10_000usize;
        let c: Arc<CachedStore> = Arc::new(cached(n, 1 << 20));
        let master: Arc<dyn BackingStore> = c.clone();
        let v = Volume::new(Box::new(ArcStore(master)), Box::new(RamOverlay::new()));

        // First volume read populates the cache from the master.
        let mut buf = [0u8; 5];
        v.read_at(100, &mut buf).unwrap();
        assert_eq!(buf, [100, 101, 102, 103, 104]);
        assert_eq!(c.stats().misses, 1);

        // Second read of the same region is served from the cache.
        v.read_at(100, &mut buf).unwrap();
        assert_eq!(buf, [100, 101, 102, 103, 104]);
        assert_eq!(c.stats().hits, 1);

        // A write goes to the overlay, never the cached master: reading it back
        // does not touch the master (miss count for that block is unchanged).
        v.write_at(200, &[0xAB; 4]).unwrap();
        let misses_before = c.stats().misses;
        let mut wbuf = [0u8; 4];
        v.read_at(200, &mut wbuf).unwrap();
        assert_eq!(wbuf, [0xAB; 4]);
        assert_eq!(c.stats().misses, misses_before);
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p xboot-core cache::`
Expected: PASS. The concurrency test relies on `CachedStore: Send + Sync` (it is — `Mutex`, `AtomicU64`, and `Box<dyn BackingStore: Send + Sync>` are all `Send + Sync`). If `volume_reads_through_cache_unmodified` sees an unexpected extra miss, check that block 0 (bytes 100 and 200 share block 0) is being reused.

- [ ] **Step 3: Commit**

```bash
git add crates/xboot-core/src/cache/mod.rs
git commit -m "test(cache): concurrent readers and Volume-over-CachedStore"
```

---

### Task 5: Final verification gate

Run the full quality gate from the spec's "Готово, когда" (Done when) section.

**Files:** none (verification only).

- [ ] **Step 1: Format check**

Run: `cargo fmt --all`
Then: `cargo fmt --all --check`
Expected: no output (clean). If `--check` reports diffs, the first `fmt` already fixed them; re-run `--check` to confirm clean.

- [ ] **Step 2: Clippy with warnings denied**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: finishes with no warnings/errors. Common fix if it flags the cache: the `budget` field is now used (Task 2), so the earlier `dead_code` warning is gone.

- [ ] **Step 3: Full test suite**

Run: `cargo test --all`
Expected: all tests pass, including the new `cache::` unit tests, `cache::prop`, and every pre-existing `storage`/`volume`/`config` test.

- [ ] **Step 4: Commit any formatting fixups**

Only if Step 1 changed files:

```bash
git add -A
git commit -m "style(cache): rustfmt fixups for phase 04"
```

If nothing changed, skip this commit.

---

## Self-Review

**Spec coverage check:**

- §1 Component/placement — `CachedStore` decorator over `Box<dyn BackingStore>`, shared via `Arc` + `ArcStore`, reuses `crate::volume::BLOCK`, constructor `new(inner, budget_bytes)`: Task 1 (struct/new) + Task 4 (`ArcStore` sharing). ✅
- §2 Read path (hit copies sub-range under lock; miss unlocks, reads full block, re-locks, inserts, copies): Task 2 `read_at`. ✅
- §3 Budget/eviction (`unbounded` LRU, manual `current_bytes`, `while > budget pop_lru`, insert only if `block_len <= budget`, invariant `<= budget`): Task 2 `LruState::insert` + gate; covered by `byte_budget_is_never_exceeded`, `zero_budget…`, `budget_smaller_than_block…`. ✅
- §4 Concurrency (one `Mutex`, lock not held during I/O, benign duplicate misses, `Send + Sync`, `Arc` sharing): Task 2 `read_at` structure + Task 4 `concurrent_readers…`. ✅
- §5 Stats (`AtomicU64` hits/misses outside lock; blocks/bytes under lock; `CacheStats`): Task 1 `stats()`/`CacheStats`. ✅
- §6 Errors (propagate `inner` `Err`, cache nothing; trust bounds contract — no re-validation): Task 2 `read_at` uses `?` before any insert and performs no bounds checks. ✅
- §7 Tests (hit/miss, read-through small/zero budget, byte-correctness, budget, LRU order, proptest, concurrency): Tasks 1–4. ✅
- "Done when" gate (`fmt --check`, `clippy -D warnings`, `cargo test --all`): Task 5. ✅
- Dependency: add `lru` to `crates/xboot-core/Cargo.toml`: Task 1 Step 1. ✅

**Out of scope (correctly omitted):** writeback-overlay cache, mutex sharding/lock-free, count-based eviction, prefetch/readahead, cache persistence (spec "Вне области Phase 04").

**Type/name consistency:** `CachedStore`, `CacheStats { hits: u64, misses: u64, blocks: usize, bytes: u64 }`, `LruState { cache, current_bytes }`, `LruState::insert(&mut self, blk: u64, data: Box<[u8]>, budget: u64)`, `CachedStore::new(inner: Box<dyn BackingStore>, budget_bytes: u64)`, `stats(&self) -> CacheStats` — used identically across Tasks 1–5. `lru` API used: `LruCache::unbounded()`, `.get(&k) -> Option<&V>`, `.put(k, v) -> Option<V>`, `.pop_lru() -> Option<(K, V)>`, `.len()`. ✅

**Placeholder scan:** none — every code/test step carries complete code; every run step states the exact command and expected result.
