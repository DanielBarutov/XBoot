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
