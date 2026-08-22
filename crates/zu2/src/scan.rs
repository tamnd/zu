//! The scan plane: the key set in order, so a range scan has somewhere
//! to run.
//!
//! The hash index answers a point read in a cacheline probe and one
//! dereference, and it answers nothing else. A range scan needs the
//! keys in order, and that is all it needs, because the value for a key
//! is already one probe away.
//!
//! So this holds keys and nothing else. No addresses, no values, no
//! versions. A scan walks the ordered key set and does the ordinary
//! point read for each key it reaches, which costs one probe and one
//! dereference on top of the walk and buys three things a plane holding
//! addresses would not get: nothing to keep in sync when a write
//! appends a new record, no second reference into the log for
//! compaction to have to know about, and the same consistency a point
//! read has, because the read is a point read.
//!
//! The structure is a skip list, and the reason is the shape of the
//! work. YCSB workload E seeks once and then walks fifty records, so
//! the walk is the hot path. A skip list's weakness is the point
//! lookup, log n dereferences with a cache miss at most of them, and
//! that weakness is never exercised here because the hash plane takes
//! every point read. Its strength is that once the seek has landed,
//! level zero is a linked list in key order and every further record is
//! one dereference and no comparison at all. Bf-Tree (Hao and
//! Chandramouli, PVLDB 17(11), 2024) and FB+-tree are both solving the
//! harder problem of serving lookups and scans out of one structure,
//! which is not the problem here.
//!
//! An adaptive radix tree would hold the same key set in far less
//! memory, since a trie stores a shared prefix once and a skip list
//! stores it in every node, and PermART (Khalaji, Brown and Daudjee,
//! SIGMOD 2026) is the current state of doing that under concurrency.
//! It is the right follow up and #548 says so. What it is not is the
//! version that can be measured this week, and the multiversion
//! machinery it needs for a linearizable range query is machinery zu2
//! already has in the log.
//!
//! Nothing is ever unlinked. A delete writes a tombstone into the log
//! and leaves the key here, and the scan skips it when the record comes
//! back tombstoned. That is what makes the whole structure safe without
//! a reclamation scheme: the memory reclamation problem the concurrent
//! index literature spends its time on is a scan holding references
//! across nodes a writer wants to free, and no writer here ever wants
//! to free one. The cost is a node for a key that was deleted and never
//! written again, bounded by the keys the database has ever held, and
//! [`Ordered::bytes`] reports it rather than hiding it.

use std::slice;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::error::{Error, Result};

/// Levels a node can have. A quarter of the nodes reach each level up,
/// so twenty covers a key set of four to the twentieth, which is more
/// keys than the address space holds records.
const MAX_HEIGHT: usize = 20;

/// How much the arena takes at a time. Big enough that a chunk is
/// thousands of nodes and the bump is uncontended in practice, small
/// enough that a database with a hundred keys does not hold a
/// megabyte per level of anything.
const CHUNK: usize = 1 << 20;

/// How many chunks the arena can hold. The bump state packs the chunk
/// index into 32 bits, so this is a policy number rather than a limit
/// of the encoding, and at a megabyte each it is sixty four gigabytes
/// of nodes.
const CHUNKS: usize = 1 << 16;

/// A bump allocator over chunks that never move.
///
/// Nodes are variable length and are never freed one at a time, which
/// is the whole reason this is not a `Box` per node: a node is its
/// header, its forward pointers and its key bytes contiguous, so a walk
/// touches one allocation per step instead of two, and the arena is
/// dropped whole when the database closes.
struct Arena {
    /// Base of each chunk, so a bump can find its chunk with one
    /// atomic load rather than by taking the lock the chunk list is
    /// under.
    bases: Box<[AtomicU64]>,
    /// The live chunk index in the high 32 bits and the offset into it
    /// in the low 32.
    state: AtomicU64,
    /// What has to be freed, and the lock a new chunk is added under.
    owned: Mutex<Vec<Box<[u8]>>>,
    bytes: AtomicUsize,
}

fn pack(index: usize, offset: usize) -> u64 {
    ((index as u64) << 32) | offset as u64
}

impl Arena {
    fn new() -> Self {
        let bases: Box<[AtomicU64]> = (0..CHUNKS).map(|_| AtomicU64::new(0)).collect();
        let mut first = vec![0u8; CHUNK].into_boxed_slice();
        bases[0].store(first.as_mut_ptr() as u64, Ordering::Release);
        Arena {
            bases,
            state: AtomicU64::new(pack(0, 0)),
            owned: Mutex::new(vec![first]),
            bytes: AtomicUsize::new(CHUNK),
        }
    }

    /// Eight byte aligned space for `size` bytes, zeroed.
    ///
    /// A request larger than a chunk gets a chunk of its own and does
    /// not disturb the bump, which keeps a key that happens to be huge
    /// from wasting the tail of the live chunk for everybody else.
    fn alloc(&self, size: usize) -> Result<*mut u8> {
        let size = size.next_multiple_of(8);
        if size > CHUNK {
            return self.private(size);
        }
        loop {
            let current = self.state.load(Ordering::Acquire);
            let index = (current >> 32) as usize;
            let offset = (current & 0xffff_ffff) as usize;
            if offset + size <= CHUNK {
                if self
                    .state
                    .compare_exchange_weak(
                        current,
                        pack(index, offset + size),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    let base = self.bases[index].load(Ordering::Acquire) as *mut u8;
                    // SAFETY: the base was published before the state
                    // that names its index, and the compare and swap
                    // above is what makes this range nobody else's.
                    return Ok(unsafe { base.add(offset) });
                }
                continue;
            }
            self.grow(index)?;
        }
    }

    /// Puts a fresh chunk in, unless somebody already has.
    fn grow(&self, seen: usize) -> Result<()> {
        let mut owned = self.owned.lock().unwrap_or_else(|e| e.into_inner());
        if (self.state.load(Ordering::Acquire) >> 32) as usize != seen {
            return Ok(());
        }
        if seen + 1 == CHUNKS {
            return Err(Error::ArenaFull { max: CHUNKS });
        }
        let mut chunk = vec![0u8; CHUNK].into_boxed_slice();
        self.bases[seen + 1].store(chunk.as_mut_ptr() as u64, Ordering::Release);
        owned.push(chunk);
        self.bytes.fetch_add(CHUNK, Ordering::Relaxed);
        self.state.store(pack(seen + 1, 0), Ordering::Release);
        Ok(())
    }

    fn private(&self, size: usize) -> Result<*mut u8> {
        let mut owned = self.owned.lock().unwrap_or_else(|e| e.into_inner());
        let mut chunk = vec![0u8; size].into_boxed_slice();
        let at = chunk.as_mut_ptr();
        owned.push(chunk);
        self.bytes.fetch_add(size, Ordering::Relaxed);
        Ok(at)
    }

    fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

// SAFETY: everything mutable is either atomic or behind the mutex, and
// the pointers handed out are into chunks that never move and are only
// freed when the arena is, which needs an owning reference.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

/// A node, as bytes. Key length, then height, then one forward pointer
/// per level, then the key.
///
/// Laid out by hand rather than as a struct because the tail is two
/// variable length arrays, and a `Box<Node>` with the key beside it
/// would put a second dereference on the walk this whole plane exists
/// to make cheap.
const KEY_LEN: usize = 0;
const HEIGHT: usize = 4;
const LINKS: usize = 8;

/// SAFETY for all of these: `at` came from [`Ordered::node`] or from a
/// forward pointer of one, so it points at a whole node in a chunk that
/// outlives the list.
unsafe fn height_of(at: *const u8) -> usize {
    unsafe { at.add(HEIGHT).cast::<u32>().read() as usize }
}

unsafe fn key_of<'a>(at: *const u8) -> &'a [u8] {
    unsafe {
        let len = at.add(KEY_LEN).cast::<u32>().read() as usize;
        let start = LINKS + 8 * height_of(at);
        slice::from_raw_parts(at.add(start), len)
    }
}

unsafe fn link(at: *const u8, level: usize) -> &'static AtomicU64 {
    unsafe { &*at.add(LINKS + 8 * level).cast::<AtomicU64>() }
}

unsafe fn next_of(at: *const u8, level: usize) -> *const u8 {
    unsafe { link(at, level).load(Ordering::Acquire) as *const u8 }
}

/// The key set in order.
pub struct Ordered {
    arena: Arena,
    /// The sentinel. Full height, no key, allocated once and never
    /// replaced, so this is a plain pointer rather than an atomic one.
    head: *const u8,
    keys: AtomicUsize,
    /// Feeds the height of the next node. A counter rather than a
    /// random source, mixed, because there is no random source here and
    /// a mixed counter gives the same geometric distribution without
    /// one.
    seed: AtomicU64,
}

// SAFETY: the only mutation is the atomic links inside the arena's
// chunks and the arena's own bump, and the head pointer is written once
// before the list is shared.
unsafe impl Send for Ordered {}
unsafe impl Sync for Ordered {}

impl Ordered {
    pub fn new() -> Result<Self> {
        let arena = Arena::new();
        let head = node(&arena, &[], MAX_HEIGHT)?;
        Ok(Ordered {
            arena,
            head,
            keys: AtomicUsize::new(0),
            seed: AtomicU64::new(0x2545_F491_4F6C_DD1D),
        })
    }

    /// How many distinct keys have been inserted, which is not how many
    /// are live: a deleted key keeps its node. See the module note.
    pub fn keys(&self) -> usize {
        self.keys.load(Ordering::Relaxed)
    }

    /// What the plane costs in memory, chunks reserved rather than
    /// bytes used, because the reserved figure is what the process is
    /// holding.
    pub fn bytes(&self) -> usize {
        self.arena.bytes()
    }

    /// One over four, geometrically, capped at [`MAX_HEIGHT`].
    fn height(&self) -> usize {
        let mut x = self
            .seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        let mut height = 1;
        while height < MAX_HEIGHT && x & 3 == 0 {
            height += 1;
            x >>= 2;
        }
        height
    }

    /// The node before `key` at every level, and the one at or after it.
    /// Answers whether the key is already there.
    fn find(
        &self,
        key: &[u8],
        before: &mut [*const u8; MAX_HEIGHT],
        after: &mut [*const u8; MAX_HEIGHT],
    ) -> bool {
        let mut at = self.head;
        for level in (0..MAX_HEIGHT).rev() {
            loop {
                // SAFETY: as on the accessors. The head has full height,
                // so every level is a real link on the first step, and
                // a node reached from level `level` has at least that
                // many links because that is how it got linked there.
                let next = unsafe { next_of(at, level) };
                if next.is_null() || unsafe { key_of(next) } >= key {
                    break;
                }
                at = next;
            }
            before[level] = at;
            after[level] = unsafe { next_of(at, level) };
        }
        !after[0].is_null() && unsafe { key_of(after[0]) } == key
    }

    /// Puts `key` in if it is not there. Answers whether it went in.
    pub fn insert(&self, key: &[u8]) -> Result<bool> {
        let mut before = [std::ptr::null(); MAX_HEIGHT];
        let mut after = [std::ptr::null(); MAX_HEIGHT];
        if self.find(key, &mut before, &mut after) {
            return Ok(false);
        }
        let height = self.height();
        let fresh = node(&self.arena, key, height)?;
        loop {
            // The level zero link is the linearisation point, so the
            // find has to be redone against it and not against whatever
            // the list looked like before the node was allocated. A
            // node that loses this race twice is abandoned in the
            // arena, which costs its bytes until the database closes
            // and costs nothing else.
            if self.find(key, &mut before, &mut after) {
                return Ok(false);
            }
            // SAFETY: as on the accessors.
            unsafe {
                link(fresh, 0).store(after[0] as u64, Ordering::Release);
                if link(before[0], 0)
                    .compare_exchange(
                        after[0] as u64,
                        fresh as u64,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    break;
                }
            }
        }
        self.keys.fetch_add(1, Ordering::Relaxed);
        // The node is reachable now, at level zero, which is the level
        // a scan walks. The rest is the index over it, and a reader
        // that misses it at a higher level finds it on the way down.
        for level in 1..height {
            loop {
                // SAFETY: as on the accessors.
                unsafe {
                    link(fresh, level).store(after[level] as u64, Ordering::Release);
                    if link(before[level], level)
                        .compare_exchange(
                            after[level] as u64,
                            fresh as u64,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
                self.find(key, &mut before, &mut after);
            }
        }
        Ok(true)
    }

    /// Whether `key` is in the plane. For tests and for the checkpoint
    /// to check itself with, not for the read path, which goes through
    /// the hash index.
    pub fn contains(&self, key: &[u8]) -> bool {
        let mut before = [std::ptr::null(); MAX_HEIGHT];
        let mut after = [std::ptr::null(); MAX_HEIGHT];
        self.find(key, &mut before, &mut after)
    }

    /// A walk starting at the first key at or after `start`.
    pub fn seek(&self, start: &[u8]) -> Cursor<'_> {
        let mut before = [std::ptr::null(); MAX_HEIGHT];
        let mut after = [std::ptr::null(); MAX_HEIGHT];
        self.find(start, &mut before, &mut after);
        Cursor {
            list: std::marker::PhantomData,
            at: after[0],
        }
    }

    /// A builder for a plane being filled from a key set that is
    /// already in order, which is what a checkpoint restore has.
    ///
    /// [`Ordered::insert`] costs a seek from the head per key, and a
    /// seek is log N dereferences with a cache miss at most of them. A
    /// restore does not need any of that: it knows the key is above
    /// every key already in, so the node it links after is the last
    /// node it linked at that level, which it can just remember. That
    /// makes the build one allocation and a handful of stores a key
    /// with no comparisons at all.
    ///
    /// Only valid on a plane nothing else is touching, which the open
    /// is, and only valid for keys handed over in strictly ascending
    /// order, which a checkpoint's key section is by construction.
    pub(crate) fn builder(&self) -> Builder<'_> {
        Builder {
            list: self,
            tails: [self.head; MAX_HEIGHT],
        }
    }

    /// A walk from the first key.
    pub fn first(&self) -> Cursor<'_> {
        Cursor {
            list: std::marker::PhantomData,
            // SAFETY: the head is a full height node in the arena.
            at: unsafe { next_of(self.head, 0) },
        }
    }
}

/// Fills a plane from keys already in order. See [`Ordered::builder`].
pub(crate) struct Builder<'a> {
    list: &'a Ordered,
    /// The last node linked at each level, which is the node the next
    /// key links after. Starts as the head at every level, which is
    /// what an empty list's last node is.
    tails: [*const u8; MAX_HEIGHT],
}

impl Builder<'_> {
    /// Puts `key` on the end. The caller owes it that `key` is above
    /// every key handed over before it; nothing here checks, because
    /// the check is the comparison this exists to avoid.
    pub(crate) fn push(&mut self, key: &[u8]) -> Result<()> {
        let height = self.list.height();
        let fresh = node(&self.list.arena, key, height)?;
        for (level, tail) in self.tails.iter_mut().enumerate().take(height) {
            // SAFETY: as on the accessors. Every tail is either the
            // head, which has full height, or a node linked at this
            // level, which therefore has at least this many links.
            unsafe { link(*tail, level).store(fresh as u64, Ordering::Release) };
            *tail = fresh;
        }
        self.list.keys.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Space for a node and its key, with the header filled in and the
/// links left null.
fn node(arena: &Arena, key: &[u8], height: usize) -> Result<*const u8> {
    let size = LINKS + 8 * height + key.len();
    let at = arena.alloc(size)?;
    // SAFETY: the arena just handed back `size` zeroed and aligned
    // bytes that nobody else has, which is exactly this layout.
    unsafe {
        at.add(KEY_LEN).cast::<u32>().write(key.len() as u32);
        at.add(HEIGHT).cast::<u32>().write(height as u32);
        std::ptr::copy_nonoverlapping(key.as_ptr(), at.add(LINKS + 8 * height), key.len());
    }
    Ok(at)
}

/// A position in the key order.
///
/// Holds no lock and takes no epoch, because nothing is ever unlinked
/// and nothing is ever freed while the list is alive. A cursor is
/// therefore as valid as the list it came from and no more careful than
/// that.
pub struct Cursor<'a> {
    /// Ties the cursor to the list, which is what keeps the arena the
    /// nodes are in alive for as long as the keys handed out are.
    list: std::marker::PhantomData<&'a Ordered>,
    at: *const u8,
}

impl<'a> Cursor<'a> {
    /// The key under the cursor, or nothing at the end of the order.
    pub fn key(&self) -> Option<&'a [u8]> {
        if self.at.is_null() {
            return None;
        }
        // SAFETY: the node is in the list's arena, which outlives 'a.
        Some(unsafe { key_of(self.at) })
    }

    /// Moves to the next key. One dereference, no comparison, which is
    /// the whole point of the plane.
    pub fn step(&mut self) {
        if self.at.is_null() {
            return;
        }
        // SAFETY: as above, and every node has a level zero link.
        self.at = unsafe { next_of(self.at, 0) };
    }
}

impl<'a> Iterator for Cursor<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.key()?;
        self.step();
        Some(key)
    }
}

impl std::fmt::Debug for Ordered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ordered")
            .field("keys", &self.keys())
            .field("bytes", &self.bytes())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: usize) -> Vec<u8> {
        format!("user{i:012}").into_bytes()
    }

    #[test]
    fn keys_come_back_in_order_whatever_order_they_went_in() {
        let list = Ordered::new().unwrap();
        // Scattered rather than ascending, so an implementation that
        // only ever appends to the tail fails this.
        let mut i = 1u64;
        for _ in 0..5000 {
            i = i.wrapping_mul(6364136223846793005).wrapping_add(1);
            assert!(list.insert(&key((i >> 33) as usize % 5000)).is_ok());
        }
        let walked: Vec<Vec<u8>> = list.first().map(<[u8]>::to_vec).collect();
        let mut sorted = walked.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(walked, sorted);
        assert_eq!(walked.len(), list.keys());
    }

    #[test]
    fn a_key_goes_in_once_however_many_times_it_is_offered() {
        let list = Ordered::new().unwrap();
        assert!(list.insert(b"a").unwrap());
        assert!(!list.insert(b"a").unwrap());
        assert!(!list.insert(b"a").unwrap());
        assert_eq!(list.keys(), 1);
        assert_eq!(list.first().count(), 1);
    }

    #[test]
    fn a_seek_lands_on_the_first_key_at_or_after_the_one_it_was_given() {
        let list = Ordered::new().unwrap();
        for i in (0..100).step_by(2) {
            list.insert(&key(i)).unwrap();
        }
        assert_eq!(list.seek(&key(10)).key().unwrap(), key(10).as_slice());
        // An odd key is not there, so the seek lands past it.
        assert_eq!(list.seek(&key(11)).key().unwrap(), key(12).as_slice());
        assert_eq!(list.seek(&key(0)).key().unwrap(), key(0).as_slice());
        assert!(list.seek(&key(1000)).key().is_none());
        assert!(list.seek(b"").key().is_some());
    }

    #[test]
    fn a_walk_of_fifty_from_the_middle_is_the_fifty_that_follow() {
        let list = Ordered::new().unwrap();
        for i in 0..1000 {
            list.insert(&key(i)).unwrap();
        }
        let got: Vec<Vec<u8>> = list.seek(&key(400)).take(50).map(<[u8]>::to_vec).collect();
        let want: Vec<Vec<u8>> = (400..450).map(key).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn an_empty_key_and_a_prefix_of_another_key_both_sort_where_bytes_say() {
        let list = Ordered::new().unwrap();
        for k in [&b""[..], b"a", b"ab", b"abc", b"b"] {
            list.insert(k).unwrap();
        }
        let got: Vec<Vec<u8>> = list.first().map(<[u8]>::to_vec).collect();
        assert_eq!(
            got,
            vec![
                b"".to_vec(),
                b"a".to_vec(),
                b"ab".to_vec(),
                b"abc".to_vec(),
                b"b".to_vec()
            ]
        );
    }

    #[test]
    fn a_key_larger_than_a_chunk_gets_a_chunk_of_its_own() {
        let list = Ordered::new().unwrap();
        let big = vec![b'z'; CHUNK + 17];
        assert!(list.insert(&big).unwrap());
        list.insert(b"a").unwrap();
        let got: Vec<usize> = list.first().map(<[u8]>::len).collect();
        assert_eq!(got, vec![1, CHUNK + 17]);
        assert!(list.bytes() > CHUNK);
    }

    #[test]
    fn threads_inserting_the_same_keys_leave_one_of_each_and_the_order_holds() {
        let list = std::sync::Arc::new(Ordered::new().unwrap());
        let mut threads = Vec::new();
        for t in 0..8 {
            let list = list.clone();
            threads.push(std::thread::spawn(move || {
                // Overlapping ranges, so every key is offered by more
                // than one thread and most are offered by all eight.
                for i in 0..4000 {
                    list.insert(&key((i * 7 + t * 13) % 4000)).unwrap();
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let walked: Vec<Vec<u8>> = list.first().map(<[u8]>::to_vec).collect();
        assert_eq!(walked.len(), 4000);
        assert_eq!(list.keys(), 4000);
        let want: Vec<Vec<u8>> = (0..4000).map(key).collect();
        assert_eq!(walked, want);
    }

    #[test]
    fn the_arena_keeps_growing_past_its_first_chunk() {
        let list = Ordered::new().unwrap();
        // Each node is a header, its links and a sixteen byte key, so a
        // megabyte is a few tens of thousands of them.
        for i in 0..80_000 {
            list.insert(&key(i)).unwrap();
        }
        assert!(
            list.bytes() > CHUNK,
            "still one chunk at {} bytes",
            list.bytes()
        );
        assert_eq!(list.first().count(), 80_000);
        assert!(list.contains(&key(79_999)));
        assert!(!list.contains(&key(80_000)));
    }
}
