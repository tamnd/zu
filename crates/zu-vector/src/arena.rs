//! Per-morsel bump arena (perf/02 section 2).
//!
//! Every vector buffer, bitmap, and selection in a morsel comes from one
//! arena. Allocation is a pointer bump into 256 KiB blocks; reset keeps
//! the blocks and zeroes the offsets, so a warm morsel allocates nothing
//! from the heap. Blocks are handed out zeroed once at birth and reused
//! dirty after that, which keeps every byte a buffer can see initialized.
//!
//! A `RawBuf` does not borrow the arena in the type system; the executor
//! contract is that vectors never outlive their morsel unless copied into
//! a sink. Debug builds tag each buffer with the arena generation at
//! allocation time and assert it on every access, so a vector read after
//! reset fails loudly instead of reading recycled bytes.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Largest arena block. Matches the storage block so a decoded chunk and
/// its working vectors have the same granularity.
pub const BLOCK_BYTES: usize = 256 * 1024;

/// The first block an arena takes, and the step it grows by after that:
/// each new block is twice the last until it reaches [`BLOCK_BYTES`].
///
/// An arena is born per run, and most runs are small. A point read
/// wants a few vectors of working memory and used to pay a 256 KiB
/// block for them, which on a warm read is the largest single thing the
/// whole query asks the allocator for and is large enough that the
/// allocator hands back fresh pages the query then faults in one by
/// one. Starting small and doubling costs a big query one extra block
/// allocation on the way up, which it does not notice, and saves a small
/// query the other 248 KiB, which it does.
const FIRST_BLOCK_BYTES: usize = 8 * 1024;

/// Every allocation is aligned to at least a cache line, which is also
/// the widest alignment any physical type needs.
const BLOCK_ALIGN: usize = 64;

struct Block {
    ptr: NonNull<u8>,
    bytes: usize,
}

// A block is a plain byte allocation with no thread affinity.
unsafe impl Send for Block {}

impl Block {
    fn new(bytes: usize) -> Self {
        let layout = Layout::from_size_align(bytes, BLOCK_ALIGN).expect("block layout");
        // Zeroed at birth so recycled reads see initialized memory, never
        // uninit. Reset does not re-zero; stale bytes are fine, uninit is
        // undefined behavior.
        let raw = unsafe { alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(raw) else {
            handle_alloc_error(layout)
        };
        Self { ptr, bytes }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        let layout = unsafe { Layout::from_size_align_unchecked(self.bytes, BLOCK_ALIGN) };
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

/// Bump allocator for one morsel's working memory.
pub struct MorselArena {
    blocks: Vec<Block>,
    cur: usize,
    off: usize,
    generation: Arc<AtomicU64>,
}

impl Default for MorselArena {
    fn default() -> Self {
        Self::new()
    }
}

impl MorselArena {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            cur: 0,
            off: 0,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Bump-allocate `bytes` with the given alignment (a power of two,
    /// at most 64: blocks themselves are 64-byte aligned so any larger
    /// request could not be honored from a bump offset).
    pub fn alloc(&mut self, bytes: usize, align: usize) -> RawBuf {
        assert!(
            align.is_power_of_two() && align <= BLOCK_ALIGN,
            "align {align}"
        );
        if bytes == 0 {
            return RawBuf::empty();
        }
        // Oversize requests get a dedicated block so the bump path never
        // has to think about them again. It stays owned by the arena and
        // is recycled like any other block only in the sense that it is
        // freed when the arena drops.
        if bytes > BLOCK_BYTES {
            let block = Block::new(bytes);
            let ptr = block.ptr;
            // Keep the current bump block last so cur/off stay valid.
            let at = self.cur.min(self.blocks.len());
            self.blocks.insert(at, block);
            self.cur += 1;
            return self.tag(RawBuf {
                ptr,
                len: bytes,
                #[cfg(debug_assertions)]
                tag: None,
                #[cfg(debug_assertions)]
                borrowed: false,
            });
        }
        let cap = self.blocks.get(self.cur).map_or(0, |b| b.bytes);
        let off = self.off.next_multiple_of(align);
        if self.cur >= self.blocks.len() || off + bytes > cap {
            if self.cur < self.blocks.len() && off + bytes > cap {
                self.cur += 1;
            }
            if self.cur >= self.blocks.len() {
                let want = self.next_block_bytes().max(bytes);
                self.blocks.push(Block::new(want));
                self.cur = self.blocks.len() - 1;
            }
            self.off = 0;
            return self.alloc(bytes, align);
        }
        self.off = off + bytes;
        let ptr = unsafe { NonNull::new_unchecked(self.blocks[self.cur].ptr.as_ptr().add(off)) };
        self.tag(RawBuf {
            ptr,
            len: bytes,
            #[cfg(debug_assertions)]
            tag: None,
            #[cfg(debug_assertions)]
            borrowed: false,
        })
    }

    /// The size of the next block to push: twice the last one, held
    /// between the first size and the largest. An oversize block can
    /// make the last one bigger than [`BLOCK_BYTES`], and the clamp
    /// covers that too.
    fn next_block_bytes(&self) -> usize {
        let last = self.blocks.last().map_or(0, |b| b.bytes);
        (last * 2).clamp(FIRST_BLOCK_BYTES, BLOCK_BYTES)
    }

    /// Allocate a typed buffer for `count` values.
    pub fn alloc_of<T: Pod>(&mut self, count: usize) -> RawBuf {
        self.alloc(count * size_of::<T>(), align_of::<T>())
    }

    /// O(1) reset: keep the blocks, rewind the offsets. Called once per
    /// morsel. Buffers handed out before the reset are dead; debug builds
    /// catch any survivor on its next access.
    pub fn reset(&mut self) {
        self.cur = 0;
        self.off = 0;
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Bytes currently held, for budget accounting.
    pub fn capacity(&self) -> usize {
        self.blocks.iter().map(|b| b.bytes).sum()
    }

    /// An arena off this thread's pool, or a new one when the pool is
    /// empty, handed back to the pool when it is dropped.
    ///
    /// An arena is born per run and dies with it, so a warm query that
    /// allocates nothing per morsel still pays three trips to the heap
    /// before its first row: the block list, the generation counter and
    /// the first block, which at eight kilobytes is most of what a warm
    /// point read asks the allocator for. None of that is about the run.
    /// A thread runs one worker at a time and the pool threads outlive
    /// any one query, so the arena the last query left is the arena this
    /// one wants.
    ///
    /// Reuse is safe for the same reason a reset is: taking one resets
    /// it, which moves the generation, so a buffer that outlived the
    /// query it was cut from fails its tag in debug rather than reading
    /// the next query's bytes.
    pub fn pooled() -> PooledArena {
        let arena = POOL.with(|p| p.borrow_mut().pop()).map(|mut a| {
            a.reset();
            a
        });
        PooledArena {
            arena: arena.or_else(|| Some(MorselArena::new())),
        }
    }

    /// Drops every block but the first, which is what an arena that grew
    /// for one wide query hands back so the next small one does not keep
    /// its memory.
    fn shrink(&mut self) {
        self.blocks.truncate(1);
        self.cur = 0;
        self.off = 0;
    }
}

/// Arenas one thread keeps between runs. One is the working number,
/// since a thread runs one worker at a time; the rest of the room is for
/// a run that nests, which costs a pointer each and saves that run the
/// same three allocations.
const POOL_ARENAS: usize = 4;

/// The most an arena may still hold when it comes back and be kept
/// whole. A query that grew past this grew for itself, and holding its
/// blocks on the thread would charge every later query for it, so it is
/// cut back to its first block and kept for that.
const POOL_KEEP_BYTES: usize = BLOCK_BYTES;

thread_local! {
    static POOL: std::cell::RefCell<Vec<MorselArena>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// An arena on loan from this thread's pool.
///
/// It is a [`MorselArena`] in every way that matters, through `Deref`,
/// and the only thing it adds is where it goes when it is dropped.
pub struct PooledArena {
    /// Always `Some` until the drop takes it.
    arena: Option<MorselArena>,
}

impl std::ops::Deref for PooledArena {
    type Target = MorselArena;

    fn deref(&self) -> &MorselArena {
        self.arena.as_ref().expect("arena is taken only on drop")
    }
}

impl std::ops::DerefMut for PooledArena {
    fn deref_mut(&mut self) -> &mut MorselArena {
        self.arena.as_mut().expect("arena is taken only on drop")
    }
}

impl Drop for PooledArena {
    fn drop(&mut self) {
        let Some(mut arena) = self.arena.take() else {
            return;
        };
        if arena.capacity() > POOL_KEEP_BYTES {
            arena.shrink();
        }
        // A thread on its way out has already torn its locals down, and
        // an arena that cannot be handed back is simply dropped here.
        let _ = POOL.try_with(|p| {
            let mut pool = p.borrow_mut();
            if pool.len() < POOL_ARENAS {
                pool.push(arena);
            }
        });
    }
}

impl MorselArena {
    #[cfg(debug_assertions)]
    fn tag(&self, mut buf: RawBuf) -> RawBuf {
        buf.tag = Some((
            Arc::clone(&self.generation),
            self.generation.load(Ordering::Acquire),
        ));
        buf
    }

    #[cfg(not(debug_assertions))]
    fn tag(&self, buf: RawBuf) -> RawBuf {
        buf
    }
}

/// An untyped byte buffer handed out by the arena. Unique: not Clone, so
/// mutable access cannot alias. The typed accessors are safe because the
/// arena zeroes blocks at birth and every `Pod` type tolerates any bit
/// pattern.
pub struct RawBuf {
    ptr: NonNull<u8>,
    len: usize,
    #[cfg(debug_assertions)]
    tag: Option<(Arc<AtomicU64>, u64)>,
    /// Whether the bytes belong to somebody else, which is the one
    /// thing that decides whether they may be written. Debug only,
    /// like the generation tag: it is a rule the type cannot state and
    /// a mistake worth catching where it is made.
    #[cfg(debug_assertions)]
    borrowed: bool,
}

// The buffer is plain bytes; the arena outliving it is the executor
// contract, not a thread question.
unsafe impl Send for RawBuf {}
unsafe impl Sync for RawBuf {}

impl RawBuf {
    /// A zero-length buffer not backed by any arena.
    pub fn empty() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
            #[cfg(debug_assertions)]
            tag: None,
            #[cfg(debug_assertions)]
            borrowed: false,
        }
    }

    /// A buffer over memory the arena did not hand out and does not
    /// own: a column of a registered frame, which is somebody else's
    /// allocation for as long as it stays registered.
    ///
    /// This is what makes a scan of borrowed data a scan rather than a
    /// copy. A vector built over one of these reads exactly as a vector
    /// over arena memory reads, because a buffer was never more than a
    /// pointer and a length; what differs is who frees it, and the
    /// answer is nobody here. It carries no generation tag, so the
    /// use-after-reset assert that guards arena buffers does not fire
    /// on it and cannot: the memory has nothing to do with the morsel.
    ///
    /// # Safety
    ///
    /// `ptr` must point at `len` initialized bytes, aligned for every
    /// type the buffer is read as, unwritten and unfreed by anyone for
    /// as long as this buffer and every vector built over it live. The
    /// caller keeps them alive; in the engine that is the frame
    /// registration holding the handle its columns arrived on.
    pub unsafe fn borrowed(ptr: NonNull<u8>, len: usize) -> Self {
        Self {
            ptr,
            len,
            #[cfg(debug_assertions)]
            tag: None,
            #[cfg(debug_assertions)]
            borrowed: true,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn check(&self) {
        #[cfg(debug_assertions)]
        if let Some((cell, born)) = &self.tag {
            assert_eq!(
                cell.load(Ordering::Acquire),
                *born,
                "vector buffer used after its arena was reset"
            );
        }
    }

    /// View the buffer as a typed slice. The element count is the byte
    /// length over the element width; a partial trailing element is
    /// unreachable because allocation sizes come from `alloc_of`.
    #[inline]
    pub fn as_slice<T: Pod>(&self) -> &[T] {
        self.check();
        debug_assert_eq!(self.ptr.as_ptr() as usize % align_of::<T>(), 0);
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().cast(), self.len / size_of::<T>()) }
    }

    #[inline]
    pub fn as_mut_slice<T: Pod>(&mut self) -> &mut [T] {
        self.check();
        #[cfg(debug_assertions)]
        assert!(!self.borrowed, "a borrowed buffer was written to");
        debug_assert_eq!(self.ptr.as_ptr() as usize % align_of::<T>(), 0);
        unsafe {
            std::slice::from_raw_parts_mut(self.ptr.as_ptr().cast(), self.len / size_of::<T>())
        }
    }
}

/// Types a `RawBuf` may be viewed as: fixed layout, any bit pattern valid.
///
/// # Safety
/// Implementors must be `repr(C)` or primitive, contain no padding that a
/// read could observe as uninit (the arena zeroes blocks, so padding reads
/// see zero), and tolerate every bit pattern.
pub unsafe trait Pod: Copy {}

unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for i64 {}
unsafe impl Pod for f64 {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_and_reuse() {
        let mut arena = MorselArena::new();
        let mut a = arena.alloc_of::<u64>(1024);
        a.as_mut_slice::<u64>().fill(7);
        let b = arena.alloc_of::<u64>(1024);
        assert_eq!(a.as_slice::<u64>()[1023], 7);
        assert_eq!(b.as_slice::<u64>()[0], 0, "fresh block memory is zeroed");
        let held = arena.capacity();
        arena.reset();
        let _c = arena.alloc_of::<u64>(1024);
        assert_eq!(arena.capacity(), held, "reset keeps blocks");
    }

    #[test]
    fn a_small_run_pays_a_small_block() {
        let mut arena = MorselArena::new();
        let _one_vector = arena.alloc(64, 64);
        assert_eq!(
            arena.capacity(),
            FIRST_BLOCK_BYTES,
            "a run that wants 64 bytes does not take 256 KiB"
        );
        // Spilling the first block takes one twice its size.
        let _spill = arena.alloc(FIRST_BLOCK_BYTES, 64);
        assert_eq!(arena.capacity(), FIRST_BLOCK_BYTES * 3);
        // Doubling stops at the largest block, however far it runs.
        for _ in 0..8 {
            arena.alloc(BLOCK_BYTES, 64);
        }
        assert!(
            arena.blocks.iter().all(|b| b.bytes <= BLOCK_BYTES),
            "no block grows past the cap"
        );
        assert!(
            arena.blocks.iter().any(|b| b.bytes == BLOCK_BYTES),
            "a run that keeps asking reaches the cap"
        );
    }

    #[test]
    fn oversize_gets_own_block() {
        let mut arena = MorselArena::new();
        let small = arena.alloc_of::<u64>(4);
        let big = arena.alloc(BLOCK_BYTES * 2, 64);
        let small2 = arena.alloc_of::<u64>(4);
        assert_eq!(big.len(), BLOCK_BYTES * 2);
        assert_eq!(small.as_slice::<u64>().len(), 4);
        assert_eq!(small2.as_slice::<u64>().len(), 4);
    }

    #[test]
    fn alignment_honored() {
        let mut arena = MorselArena::new();
        let _odd = arena.alloc(3, 1);
        let aligned = arena.alloc_of::<u64>(1);
        assert_eq!(aligned.as_slice::<u64>().len(), 1);
    }

    #[test]
    fn borrowed_reads_the_memory_it_was_given() {
        // A column of a registered frame: the values are read where
        // they lie, and the arena never sees them.
        let outside = [7u64, 8, 9];
        let buf = unsafe {
            RawBuf::borrowed(
                NonNull::new(outside.as_ptr() as *mut u8).expect("a real pointer"),
                size_of_val(&outside[..]),
            )
        };
        assert_eq!(buf.as_slice::<u64>(), &outside[..]);
        let mut arena = MorselArena::new();
        arena.reset();
        // Nothing about the morsel reaches it: it survives the reset
        // that kills every buffer the arena handed out.
        assert_eq!(buf.as_slice::<u64>()[2], 9);
    }

    #[test]
    #[should_panic(expected = "borrowed buffer was written")]
    #[cfg(debug_assertions)]
    fn writing_a_borrowed_buffer_asserts() {
        let outside = [7u64];
        let mut buf = unsafe {
            RawBuf::borrowed(
                NonNull::new(outside.as_ptr() as *mut u8).expect("a real pointer"),
                8,
            )
        };
        buf.as_mut_slice::<u64>()[0] = 1;
    }

    #[test]
    fn a_pooled_arena_comes_back_with_the_block_the_last_run_left() {
        // On a thread of its own, because the pool is per thread and
        // what is being checked is what one thread hands itself between
        // two runs.
        std::thread::spawn(|| {
            let first = {
                let mut arena = MorselArena::pooled();
                let _ = arena.alloc_of::<u64>(64);
                arena.blocks[0].ptr
            };
            let mut arena = MorselArena::pooled();
            assert_eq!(arena.capacity(), FIRST_BLOCK_BYTES, "the block is kept");
            assert_eq!(arena.blocks[0].ptr, first, "and it is the same block");
            // Taken means reset, so the second run bumps from the top of
            // it rather than from where the first one stopped.
            assert_eq!(arena.alloc_of::<u64>(64).as_slice::<u64>().len(), 64);
            assert_eq!(arena.capacity(), FIRST_BLOCK_BYTES, "nothing new is taken");
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn an_arena_that_grew_for_one_run_does_not_charge_the_next() {
        std::thread::spawn(|| {
            {
                let mut arena = MorselArena::pooled();
                let _ = arena.alloc(64, 64);
                for _ in 0..8 {
                    arena.alloc(BLOCK_BYTES, 64);
                }
                assert!(arena.capacity() > POOL_KEEP_BYTES, "this run went wide");
            }
            let arena = MorselArena::pooled();
            assert_eq!(
                arena.capacity(),
                FIRST_BLOCK_BYTES,
                "a wide run hands back its first block and no more"
            );
        })
        .join()
        .expect("thread");
    }

    #[test]
    #[should_panic(expected = "arena was reset")]
    #[cfg(debug_assertions)]
    fn a_buffer_that_outlived_its_run_fails_on_the_next_one() {
        // Reuse is only as safe as the reset, and this is the reset: a
        // buffer that escaped the run it was cut from reads as a use
        // after reset on the run that takes the arena next, rather than
        // quietly reading that run's bytes.
        let buf = {
            let mut arena = MorselArena::pooled();
            arena.alloc_of::<u64>(8)
        };
        let _next = MorselArena::pooled();
        let _ = buf.as_slice::<u64>();
    }

    #[test]
    #[should_panic(expected = "arena was reset")]
    #[cfg(debug_assertions)]
    fn use_after_reset_asserts() {
        let mut arena = MorselArena::new();
        let buf = arena.alloc_of::<u64>(8);
        arena.reset();
        let _ = buf.as_slice::<u64>();
    }
}
