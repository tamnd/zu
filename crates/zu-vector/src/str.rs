//! String views, buffers, and dictionaries (perf/02 sections 1.1, 1.2).
//!
//! A string cell is a fixed 16-byte view: length, a 4-byte prefix, and
//! either the remaining bytes inline (len <= 12) or a buffer id plus
//! offset into shared buffers. Comparisons check the prefix word first,
//! which settles most inequality without touching the payload. A buffer
//! is a byte range and a handle that keeps it alive: a decoded segment
//! chunk straight from storage, bytes a kernel computed, or the data
//! buffer of a registered frame, which the engine reads where it lies
//! and never owns. No String allocation and no UTF-8 revalidation
//! happen on the read path; FSST and FullZip chunks are validated once
//! at decode.

use std::sync::Arc;

use crate::arena::Pod;

/// Longest string held entirely inside the view.
pub const INLINE_LEN: usize = 12;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StrView {
    len: u32,
    prefix: [u8; 4],
    tail: [u8; 8],
}

unsafe impl Pod for StrView {}

const _: () = assert!(size_of::<StrView>() == 16);

impl StrView {
    /// Build a view over a short string, bytes stored inline.
    pub fn inline(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len() <= INLINE_LEN);
        let mut prefix = [0u8; 4];
        let mut tail = [0u8; 8];
        let head = bytes.len().min(4);
        prefix[..head].copy_from_slice(&bytes[..head]);
        if bytes.len() > 4 {
            tail[..bytes.len() - 4].copy_from_slice(&bytes[4..]);
        }
        Self {
            len: bytes.len() as u32,
            prefix,
            tail,
        }
    }

    /// Build a view over a long string living in `StrBuffers` buffer
    /// `buffer` at `offset`. The caller passes the full bytes so the
    /// prefix can be captured.
    pub fn long(bytes: &[u8], buffer: u16, offset: u32) -> Self {
        debug_assert!(bytes.len() > INLINE_LEN);
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&bytes[..4]);
        let mut tail = [0u8; 8];
        tail[..2].copy_from_slice(&buffer.to_le_bytes());
        tail[2..6].copy_from_slice(&offset.to_le_bytes());
        Self {
            len: bytes.len() as u32,
            prefix,
            tail,
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_inline(&self) -> bool {
        self.len as usize <= INLINE_LEN
    }

    fn buffer_id(&self) -> u16 {
        u16::from_le_bytes([self.tail[0], self.tail[1]])
    }

    fn offset(&self) -> u32 {
        u32::from_le_bytes([self.tail[2], self.tail[3], self.tail[4], self.tail[5]])
    }

    /// The bytes of an inline view, read out of the view itself.
    pub fn inline_bytes(&self) -> &[u8] {
        debug_assert!(self.is_inline());
        // prefix and tail are adjacent under repr(C), so the inline
        // payload is the 12 bytes starting at the prefix.
        let base = std::ptr::from_ref(self).cast::<u8>();
        unsafe { std::slice::from_raw_parts(base.add(4), self.len as usize) }
    }

    /// The string bytes. Inline views read out of the view itself;
    /// long views resolve through the shared buffers.
    pub fn bytes<'a>(&'a self, bufs: &'a StrBuffers) -> &'a [u8] {
        if self.is_inline() {
            self.inline_bytes()
        } else {
            bufs.slice(self.buffer_id(), self.offset(), self.len as usize)
        }
    }

    /// Equality without materializing either side. The length and prefix
    /// word reject most non-equal pairs before any buffer is touched.
    pub fn eq_with(&self, a_bufs: &StrBuffers, other: &StrView, b_bufs: &StrBuffers) -> bool {
        if self.len != other.len || self.prefix != other.prefix {
            return false;
        }
        if self.is_inline() {
            return self.tail == other.tail;
        }
        self.bytes(a_bufs) == other.bytes(b_bufs)
    }
}

/// One byte range long views resolve through, and whatever keeps it
/// alive.
///
/// A pointer and a length, with the owner beside them rather than in
/// the way of them. Bytes worth pointing at do not all come from the
/// engine's allocator: the columns of a registered frame are buffers
/// somebody else owns, and copying them to make them addressable here
/// would be the copy the registration exists to avoid. Resolving a
/// view is a bounds check and an add whichever kind of bytes it is.
struct Buf {
    ptr: *const u8,
    len: usize,
    /// Held, never read. What it is is what its drop is: freeing an
    /// engine allocation, or calling the release callback a frame
    /// arrived with.
    _owner: Owner,
}

/// What a buffer's bytes belong to.
enum Owner {
    /// An allocation of the engine's own.
    Held(#[allow(dead_code)] Arc<[u8]>),
    /// Bytes from outside the engine, alive for as long as this handle
    /// is. Nothing here knows what it is and nothing here needs to.
    Lent(#[allow(dead_code)] Arc<dyn std::any::Any + Send + Sync>),
}

// The bytes are immutable for as long as the owner lives and the owner
// is itself `Send + Sync`, so the pointer beside it carries no thread
// affinity of its own. Every buffer a vector reads through is read
// only: writing one is what `RawBuf::borrowed` refuses.
unsafe impl Send for Buf {}
unsafe impl Sync for Buf {}

/// Shared byte buffers backing long string views. A vector holding
/// views into a decoded segment chunk, or into a frame's Arrow data
/// buffer, keeps what it points at alive without copying it.
#[derive(Default)]
pub struct StrBuffers {
    bufs: Vec<Buf>,
}

/// A vector whose views are all short carries no buffers at all, and a
/// kernel resolving one still has to hand something to `bytes`. This is
/// that something, and being a constant it costs nothing to have.
pub static NO_BUFFERS: StrBuffers = StrBuffers::empty();

impl StrBuffers {
    pub fn new() -> Self {
        Self::default()
    }

    /// No buffers at all, in a form a static can hold.
    pub const fn empty() -> Self {
        Self { bufs: Vec::new() }
    }

    /// Register an allocation of the engine's own and get its id.
    pub fn push(&mut self, buf: Arc<[u8]>) -> u16 {
        let (ptr, len) = (buf.as_ptr(), buf.len());
        self.add(Buf {
            ptr,
            len,
            _owner: Owner::Held(buf),
        })
    }

    /// Register bytes from outside the engine and get their id.
    ///
    /// # Safety
    ///
    /// `ptr` must point at `len` initialized bytes that nobody writes
    /// or frees for as long as `owner` lives, and dropping `owner`
    /// must be what releases them. Holding it here is what makes that
    /// last as long as the views do.
    pub unsafe fn push_lent(
        &mut self,
        ptr: *const u8,
        len: usize,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> u16 {
        self.add(Buf {
            ptr,
            len,
            _owner: Owner::Lent(owner),
        })
    }

    fn add(&mut self, buf: Buf) -> u16 {
        let id = u16::try_from(self.bufs.len()).expect("string buffer count fits u16");
        self.bufs.push(buf);
        id
    }

    /// How many buffers are registered, which is the id the next one
    /// will take.
    pub fn len(&self) -> usize {
        self.bufs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bufs.is_empty()
    }

    pub fn slice(&self, id: u16, offset: u32, len: usize) -> &[u8] {
        let buf = &self.bufs[id as usize];
        let start = offset as usize;
        // The same bounds check indexing an `Arc<[u8]>` did, kept in
        // release as well: a view that has run past its buffer is a bug
        // in whoever built it, and the other answer is reading whatever
        // is next in the address space.
        assert!(
            start + len <= buf.len,
            "string view {start}..{} runs past its {}-byte buffer",
            start + len,
            buf.len
        );
        // Safe by `Buf`'s own contract: the bytes are alive while the
        // owner is, the owner is held here, and the range is checked.
        unsafe { std::slice::from_raw_parts(buf.ptr.add(start), len) }
    }
}

/// A sorted, deduplicated string dictionary, matching the on-disk dict
/// encoding's order so range predicates map to code ranges. Entry bytes
/// are concatenated; `ends[i]` is the exclusive end of entry i.
pub struct Dictionary {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl Dictionary {
    /// Build from entries that are already sorted and unique, which is
    /// what the storage dict guarantees.
    pub fn from_sorted<I, B>(entries: I) -> Self
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let mut bytes = Vec::new();
        let mut ends = Vec::new();
        for e in entries {
            let e = e.as_ref();
            debug_assert!(
                ends.is_empty() || {
                    let start = if ends.len() == 1 {
                        0
                    } else {
                        ends[ends.len() - 2] as usize
                    };
                    let prev = &bytes[start..*ends.last().unwrap() as usize];
                    prev < e
                },
                "dictionary entries must be sorted and unique"
            );
            bytes.extend_from_slice(e);
            ends.push(bytes.len() as u32);
        }
        Self { bytes, ends }
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn get(&self, code: u32) -> &[u8] {
        let i = code as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }

    /// Binary-search a value. `Ok(code)` when present; `Err(insertion)`
    /// otherwise, which range compares use directly: `x < needle` in the
    /// value domain is `code < insertion` in the code domain.
    pub fn code_of(&self, needle: &[u8]) -> Result<u32, u32> {
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.get(mid as u32).cmp(needle) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid as u32),
            }
        }
        Err(lo as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_roundtrip() {
        let bufs = StrBuffers::new();
        for s in ["", "a", "abcd", "abcdefgh", "twelve chars"] {
            let v = StrView::inline(s.as_bytes());
            assert_eq!(v.bytes(&bufs), s.as_bytes(), "{s:?}");
        }
    }

    #[test]
    fn long_roundtrip_and_eq() {
        let mut bufs = StrBuffers::new();
        let payload: Arc<[u8]> =
            Arc::from(&b"hello, this is a longer string sitting in a buffer"[..]);
        let id = bufs.push(Arc::clone(&payload));
        let a = StrView::long(&payload, id, 0);
        assert_eq!(a.bytes(&bufs), &payload[..]);
        let b = StrView::long(&payload, id, 0);
        assert!(a.eq_with(&bufs, &b, &bufs));
        let short = StrView::inline(b"hello");
        assert!(!a.eq_with(&bufs, &short, &bufs));
    }

    #[test]
    fn prefix_rejects_before_buffer_touch() {
        let mut bufs = StrBuffers::new();
        let p1: Arc<[u8]> = Arc::from(&b"aaaaaaaaaaaaaaaaaaaa"[..]);
        let p2: Arc<[u8]> = Arc::from(&b"bbbbbbbbbbbbbbbbbbbb"[..]);
        let i1 = bufs.push(Arc::clone(&p1));
        let i2 = bufs.push(Arc::clone(&p2));
        let a = StrView::long(&p1, i1, 0);
        let b = StrView::long(&p2, i2, 0);
        assert!(!a.eq_with(&bufs, &b, &bufs));
    }

    #[test]
    fn lent_bytes_read_where_they_lie() {
        // What a registered frame hands over: an allocation somebody
        // else made, pointed at rather than copied, kept alive by the
        // handle it arrived with.
        let outside: Arc<Vec<u8>> = Arc::new(b"a string long enough to need a buffer".to_vec());
        let (ptr, len) = (outside.as_ptr(), outside.len());
        let mut bufs = StrBuffers::new();
        let id = unsafe { bufs.push_lent(ptr, len, Arc::clone(&outside) as Arc<_>) };
        let view = StrView::long(&outside, id, 0);
        assert_eq!(view.bytes(&bufs), &outside[..]);
        // The buffers hold it, so the last handle out here is not the
        // last handle anywhere.
        assert_eq!(Arc::strong_count(&outside), 2);
        drop(outside);
        assert_eq!(view.bytes(&bufs), b"a string long enough to need a buffer");
    }

    #[test]
    #[should_panic(expected = "runs past its")]
    fn a_view_past_its_buffer_is_caught() {
        let mut bufs = StrBuffers::new();
        let id = bufs.push(Arc::from(&b"twenty bytes exactly"[..]));
        bufs.slice(id, 16, 8);
    }

    #[test]
    fn dictionary_codes() {
        let d = Dictionary::from_sorted(["apple", "banana", "cherry"]);
        assert_eq!(d.code_of(b"banana"), Ok(1));
        assert_eq!(d.code_of(b"blueberry"), Err(2));
        assert_eq!(d.get(2), b"cherry");
    }
}
