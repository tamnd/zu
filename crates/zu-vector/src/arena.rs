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

/// Arena block size. Matches the storage block so a decoded chunk and its
/// working vectors have the same granularity.
pub const BLOCK_BYTES: usize = 256 * 1024;

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
            });
        }
        let off = self.off.next_multiple_of(align);
        if self.cur >= self.blocks.len() || off + bytes > BLOCK_BYTES {
            if self.cur < self.blocks.len() && off + bytes > BLOCK_BYTES {
                self.cur += 1;
            }
            if self.cur >= self.blocks.len() {
                self.blocks.push(Block::new(BLOCK_BYTES));
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
        })
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
    #[should_panic(expected = "arena was reset")]
    #[cfg(debug_assertions)]
    fn use_after_reset_asserts() {
        let mut arena = MorselArena::new();
        let buf = arena.alloc_of::<u64>(8);
        arena.reset();
        let _ = buf.as_slice::<u64>();
    }
}
