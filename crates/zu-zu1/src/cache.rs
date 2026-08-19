//! The block cache under every read path (perf/04 section 1).
//!
//! Before this module existed every block read heap-allocated 256 KiB
//! and hit the file, so a warm scan paid malloc plus memcpy plus a
//! syscall per block per touch. Now [`Zu1File`](crate::file::Zu1File)
//! pins blocks through a shared [`BlockCache`]: a hit is a shard lock,
//! a map probe, and an `Arc` clone, no allocation and no copy; a miss
//! reads the whole block once into a recycled frame. Frames are 256 KiB
//! `Arc<Vec<u8>>`s, so a [`PinnedBlock`] guard keeps its frame alive
//! even after eviction drops it from the map, which is what makes the
//! guard safe to hold across other cache traffic with no invalidation
//! protocol: committed blocks never change in place, and the one path
//! that rewrites a block, [`Zu1File::write_block`], drops the cached
//! frame so the next read sees the new bytes.
//!
//! Eviction keeps the SIEVE retention rule: every hit sets a visited
//! bit, the hand clears visited slots and takes the first unvisited,
//! unpinned one. The slots live in a fixed per-shard array scanned in
//! rotation rather than SIEVE's insertion-order queue; on block traffic
//! the difference is the scan order of survivors, not what survives.
//! When every slot in a shard is pinned the read is served from a
//! transient frame that bypasses the pool, so the budget bounds the
//! cache itself and pin storms degrade to today's allocate-per-read
//! behavior instead of deadlocking.

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zu_common::{IdMap, Result};

use crate::BLOCK_SIZE;
use crate::file::{BlockPtr, NULL_BLOCK};

/// Cache budget the unit tests here run under: the 50% of the default
/// memory limit that perf/04 gives the compressed tier. Production
/// budgets come from `Zu1File::set_memory_limit`.
#[cfg(test)]
const DEFAULT_BUDGET: usize = 64 << 20;

/// Lock striping. Block pointers are sequential, so modulo spreads
/// neighboring blocks across shards. Sized for eight workers hitting
/// the cache from the morsel executor: at eight shards the mutex wait
/// dominated a parallel scan's profile, at sixty four collisions are
/// rare.
const SHARDS: usize = 64;

/// Hit, miss, and eviction counts since the cache was built. Tests use
/// these to prove a warm read never reached the file and that a
/// bounded-budget run actually evicted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

/// One cached block frame keyed by its block pointer.
#[derive(Debug)]
struct Slot {
    /// Pointer this frame holds, `NULL_BLOCK` when the last fill failed
    /// and left the frame unmapped.
    ptr: BlockPtr,
    /// SIEVE visited bit, set on hit, cleared by the hand.
    visited: bool,
    frame: Arc<Vec<u8>>,
}

#[derive(Debug, Default)]
struct Shard {
    /// Block pointer to slot index; the map is the source of truth,
    /// `Slot::ptr` only names what to unmap on eviction.
    map: IdMap<BlockPtr, usize>,
    slots: Vec<Slot>,
    hand: usize,
}

/// The shared block cache. Interior locking throughout, so handles
/// forked off one [`Zu1File`](crate::file::Zu1File) share it across
/// threads.
pub struct BlockCache {
    shards: [Mutex<Shard>; SHARDS],
    /// Frame cap per shard; the whole cache holds at most
    /// `SHARDS * per_shard * BLOCK_SIZE` bytes of frames.
    per_shard: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl std::fmt::Debug for BlockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockCache")
            .field("budget", &(self.per_shard * SHARDS * BLOCK_SIZE as usize))
            .field("stats", &self.stats())
            .finish()
    }
}

impl BlockCache {
    /// A cache holding at most `budget` bytes of block frames, at least
    /// one frame per shard so tiny budgets still cache something.
    pub fn new(budget: usize) -> Self {
        let frames = (budget / BLOCK_SIZE as usize).max(SHARDS);
        Self {
            shards: std::array::from_fn(|_| Mutex::new(Shard::default())),
            per_shard: frames.div_ceil(SHARDS),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    /// The cached frame for `ptr`, pinned, or `None` on a miss.
    pub(crate) fn get(&self, ptr: BlockPtr) -> Option<PinnedBlock> {
        let mut shard = self.shards[ptr as usize % SHARDS].lock().unwrap();
        let i = match shard.map.get(&ptr) {
            Some(&i) => i,
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let slot = &mut shard.slots[i];
        slot.visited = true;
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(PinnedBlock(Arc::clone(&slot.frame)))
    }

    /// Fills a frame for `ptr` through `fill` and pins it. Grows the
    /// shard until its cap, then recycles the frame SIEVE picks; with
    /// every slot pinned the frame is transient and never enters the
    /// map. The shard stays locked across `fill`, so concurrent misses
    /// on one shard serialize instead of reading the same block twice.
    pub(crate) fn insert(
        &self,
        ptr: BlockPtr,
        fill: impl FnOnce(&mut [u8]) -> Result<()>,
    ) -> Result<PinnedBlock> {
        let mut shard = self.shards[ptr as usize % SHARDS].lock().unwrap();
        if let Some(&i) = shard.map.get(&ptr) {
            // Lost a race with another filler between get and insert.
            let slot = &mut shard.slots[i];
            slot.visited = true;
            return Ok(PinnedBlock(Arc::clone(&slot.frame)));
        }
        let i = if shard.slots.len() < self.per_shard {
            shard.slots.push(Slot {
                ptr: NULL_BLOCK,
                visited: false,
                frame: Arc::new(vec![0u8; BLOCK_SIZE as usize]),
            });
            shard.slots.len() - 1
        } else {
            match self.evict(&mut shard) {
                Some(i) => i,
                None => {
                    // Every frame is pinned: serve past the pool.
                    let mut frame = vec![0u8; BLOCK_SIZE as usize];
                    fill(&mut frame)?;
                    return Ok(PinnedBlock(Arc::new(frame)));
                }
            }
        };
        let slot = &mut shard.slots[i];
        slot.ptr = NULL_BLOCK;
        let frame = Arc::get_mut(&mut slot.frame).expect("evicted frame is unpinned");
        fill(frame)?;
        slot.ptr = ptr;
        slot.visited = false;
        let pin = PinnedBlock(Arc::clone(&slot.frame));
        shard.map.insert(ptr, i);
        Ok(pin)
    }

    /// Drops the cached frame for `ptr` so the next read refills from
    /// the file. Live pins keep the old frame alive but the map forgets
    /// it, which is exactly the MVCC contract: a pin taken before a
    /// rewrite keeps reading the bytes it pinned.
    pub(crate) fn remove(&self, ptr: BlockPtr) {
        let mut shard = self.shards[ptr as usize % SHARDS].lock().unwrap();
        if let Some(i) = shard.map.remove(&ptr) {
            shard.slots[i].ptr = NULL_BLOCK;
            shard.slots[i].visited = false;
        }
    }

    /// SIEVE hand: clear visited bits until an unvisited, unpinned slot
    /// turns up, unmap it, and hand its index back. Two full sweeps
    /// finding nothing means every slot is pinned.
    fn evict(&self, shard: &mut Shard) -> Option<usize> {
        for _ in 0..2 * shard.slots.len() {
            let i = shard.hand;
            shard.hand = (shard.hand + 1) % shard.slots.len();
            let slot = &mut shard.slots[i];
            if slot.visited {
                slot.visited = false;
                continue;
            }
            if Arc::strong_count(&slot.frame) > 1 {
                continue;
            }
            if slot.ptr != NULL_BLOCK {
                shard.map.remove(&slot.ptr);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
            slot.ptr = NULL_BLOCK;
            return Some(i);
        }
        None
    }
}

/// Memory footprint of a pooled decoded object, what a
/// [`DecodedPool`]'s budget counts.
pub trait PoolBytes {
    fn pool_bytes(&self) -> usize;
}

impl PoolBytes for Vec<u64> {
    fn pool_bytes(&self) -> usize {
        self.len() * 8
    }
}

struct PoolEntry<T> {
    value: Arc<T>,
    bytes: usize,
    /// Last-touch tick for LRU eviction.
    tick: u64,
}

struct PoolInner<T> {
    map: IdMap<BlockPtr, PoolEntry<T>>,
    bytes: usize,
    tick: u64,
}

/// A budgeted pool of decoded objects keyed by the first block pointer
/// of the segment they decode, the decode-on-touch tier of perf/04
/// section 2. A dropped table's keys stop being touched and age out;
/// the one real hazard is the free list recycling a pointer into a new
/// segment, which `Zu1File::write_block` covers by calling
/// [`Self::remove`] on every block it rewrites. Values are `Arc`-handed:
/// eviction drops the map's reference and any reader still holding the
/// `Arc` keeps its snapshot alive, which is safe because decoded
/// objects are immutable under MVCC.
///
/// Eviction is plain LRU by touch tick, scanned on demand. Pools hold
/// tens of entries, whole decoded groups and directories, so the scan
/// costs less than the bookkeeping a linked LRU would add.
pub struct DecodedPool<T> {
    inner: Mutex<PoolInner<T>>,
    budget: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl<T> std::fmt::Debug for DecodedPool<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedPool")
            .field("budget", &self.budget)
            .field("stats", &self.stats())
            .finish()
    }
}

impl<T> DecodedPool<T> {
    /// A pool holding at most `budget` bytes of decoded objects. A
    /// single object above the budget is still admitted alone, so a
    /// pool cannot starve its own hot path.
    pub fn new(budget: usize) -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                map: IdMap::default(),
                bytes: 0,
                tick: 0,
            }),
            budget,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    /// The pooled object for `key`, or `None` on a miss.
    pub fn get(&self, key: BlockPtr) -> Option<Arc<T>> {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;
        match inner.map.get_mut(&key) {
            Some(entry) => {
                entry.tick = tick;
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(Arc::clone(&entry.value))
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Drops the entry under `key`, the write-invalidation hook: the
    /// free list recycles block pointers, so a rewrite of a segment's
    /// first block can give a new segment an old key. Readers still
    /// holding the old `Arc` keep their snapshot, same as eviction.
    pub fn remove(&self, key: BlockPtr) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.map.remove(&key) {
            inner.bytes -= entry.bytes;
        }
    }

    /// Pools `value` under `key`, evicting least-recently-touched
    /// entries until the budget holds it. Racing inserts keep the
    /// first entry; both racers already hold a usable `Arc`.
    pub fn insert(&self, key: BlockPtr, value: Arc<T>)
    where
        T: PoolBytes,
    {
        let bytes = value.pool_bytes();
        let mut inner = self.inner.lock().unwrap();
        if inner.map.contains_key(&key) {
            return;
        }
        while inner.bytes + bytes > self.budget && !inner.map.is_empty() {
            let &oldest = inner
                .map
                .iter()
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k)
                .unwrap();
            let evicted = inner.map.remove(&oldest).unwrap();
            inner.bytes -= evicted.bytes;
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        inner.tick += 1;
        let tick = inner.tick;
        inner.bytes += bytes;
        inner.map.insert(key, PoolEntry { value, bytes, tick });
    }
}

/// A pinned 256 KiB block frame. Holding it keeps the frame alive; the
/// cache may evict the block meanwhile, which only means the next
/// reader refills. Dereferences to the full block's bytes.
#[derive(Debug, Clone)]
pub struct PinnedBlock(Arc<Vec<u8>>);

impl Deref for PinnedBlock {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

/// Payload bytes handed out of a segment read: a borrowed window into
/// one pinned block when the span stays inside it, which is the common
/// case since writers prefer contiguous runs, or an assembled copy when
/// the span crosses blocks. Dereferences to the span's bytes either
/// way, so readers do not care which they got.
#[derive(Debug)]
pub enum SegmentBytes {
    Pinned {
        block: PinnedBlock,
        start: usize,
        len: usize,
    },
    Owned(Vec<u8>),
}

impl Deref for SegmentBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Pinned { block, start, len } => &block[*start..start + len],
            Self::Owned(bytes) => bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_with(byte: u8) -> impl FnOnce(&mut [u8]) -> Result<()> {
        move |buf| {
            buf.fill(byte);
            Ok(())
        }
    }

    #[test]
    fn hit_returns_the_inserted_bytes() {
        let cache = BlockCache::new(DEFAULT_BUDGET);
        assert!(cache.get(7).is_none());
        let pin = cache.insert(7, fill_with(0xAB)).unwrap();
        assert!(pin.iter().all(|&b| b == 0xAB));
        drop(pin);
        let pin = cache.get(7).expect("warm hit");
        assert!(pin.iter().all(|&b| b == 0xAB));
        let s = cache.stats();
        assert_eq!((s.hits, s.misses), (1, 1));
    }

    #[test]
    fn remove_forces_a_refill() {
        let cache = BlockCache::new(DEFAULT_BUDGET);
        cache.insert(3, fill_with(1)).unwrap();
        cache.remove(3);
        assert!(cache.get(3).is_none());
        let pin = cache.insert(3, fill_with(2)).unwrap();
        assert_eq!(pin[0], 2);
    }

    /// Pointers that all land in shard 0 no matter the shard count.
    const P1: u64 = SHARDS as u64;
    const P2: u64 = 2 * SHARDS as u64;
    const P3: u64 = 3 * SHARDS as u64;

    #[test]
    fn eviction_respects_pins_and_bounds_frames() {
        // Budget of one frame per shard; pointers all land in shard 0.
        let cache = BlockCache::new(0);
        let pinned = cache.insert(P1, fill_with(1)).unwrap();
        // The only slot is pinned, so this read bypasses the pool and
        // evicts nothing.
        let transient = cache.insert(P2, fill_with(2)).unwrap();
        assert_eq!(transient[0], 2);
        assert_eq!(cache.stats().evictions, 0);
        assert!(cache.get(P2).is_none(), "transient frames are not cached");
        assert_eq!(pinned[0], 1);
        drop(pinned);
        drop(transient);
        // Unpinned now: P2 takes P1's frame.
        cache.insert(P2, fill_with(3)).unwrap();
        assert_eq!(cache.stats().evictions, 1);
        assert!(cache.get(P1).is_none());
        assert_eq!(cache.get(P2).unwrap()[0], 3);
    }

    #[test]
    fn visited_bit_gives_hot_blocks_a_second_chance() {
        let cache = BlockCache::new(0);
        cache.insert(P1, fill_with(1)).unwrap();
        // Not visited since insert: the hand takes it directly.
        cache.insert(P2, fill_with(2)).unwrap();
        assert!(cache.get(P1).is_none());
        // Visit P2, then insert P3: the hand clears P2's bit and keeps
        // scanning, but with one slot it wraps and takes it anyway;
        // with two slots in the shard the visited one survives.
        let cache = BlockCache::new(2 * SHARDS * BLOCK_SIZE as usize);
        cache.insert(P1, fill_with(1)).unwrap();
        cache.insert(P2, fill_with(2)).unwrap();
        cache.get(P2).unwrap();
        cache.insert(P3, fill_with(3)).unwrap();
        assert!(cache.get(P2).is_some(), "visited slot survives");
        assert!(cache.get(P1).is_none(), "unvisited slot was the victim");
    }

    #[test]
    fn pool_serves_arcs_and_keeps_the_first_of_racing_inserts() {
        let pool: DecodedPool<Vec<u64>> = DecodedPool::new(1 << 20);
        assert!(pool.get(8).is_none());
        pool.insert(8, Arc::new(vec![1, 2, 3]));
        pool.insert(8, Arc::new(vec![9, 9, 9]));
        assert_eq!(*pool.get(8).unwrap(), vec![1, 2, 3]);
        let s = pool.stats();
        assert_eq!((s.hits, s.misses, s.evictions), (1, 1, 0));
    }

    #[test]
    fn pool_evicts_least_recently_touched_within_budget() {
        // Budget fits two of the three 80-byte vectors.
        let pool: DecodedPool<Vec<u64>> = DecodedPool::new(160);
        pool.insert(8, Arc::new(vec![1; 10]));
        pool.insert(16, Arc::new(vec![2; 10]));
        // Touch 16 then 8, so 16 is the LRU entry when 24 arrives.
        let held = pool.get(16).unwrap();
        pool.get(8).unwrap();
        pool.insert(24, Arc::new(vec![3; 10]));
        assert!(pool.get(16).is_none(), "LRU entry evicted");
        assert!(pool.get(8).is_some());
        assert!(pool.get(24).is_some());
        // Eviction only dropped the map's reference; the reader's Arc
        // still holds the values.
        assert_eq!(*held, vec![2; 10]);
        assert_eq!(pool.stats().evictions, 1);
    }

    #[test]
    fn pool_admits_an_oversized_object_alone() {
        let pool: DecodedPool<Vec<u64>> = DecodedPool::new(64);
        pool.insert(8, Arc::new(vec![1; 4]));
        pool.insert(16, Arc::new(vec![2; 100]));
        assert!(pool.get(8).is_none(), "everything else made way");
        assert_eq!(pool.get(16).unwrap().len(), 100);
    }

    #[test]
    fn segment_bytes_deref_both_ways() {
        let cache = BlockCache::new(DEFAULT_BUDGET);
        let pin = cache.insert(5, fill_with(9)).unwrap();
        let pinned = SegmentBytes::Pinned {
            block: pin,
            start: 10,
            len: 4,
        };
        assert_eq!(&pinned[..], &[9, 9, 9, 9]);
        let owned = SegmentBytes::Owned(vec![1, 2, 3]);
        assert_eq!(&owned[..], &[1, 2, 3]);
    }
}
