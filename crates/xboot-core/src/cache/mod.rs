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
}

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
