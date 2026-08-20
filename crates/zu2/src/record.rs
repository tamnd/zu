//! What one record looks like on the log.
//!
//! ```text
//! previous  u64   address of the prior version of this key, or NULL
//! version   u64   commit sequence number, bit 63 is the in-place lock
//! key_len   u32   bit 31 is the tombstone flag
//! value_len u32
//! crc       u32   crc32c over the header above and both byte runs
//! kind      u32   what the record is: a keyed value, or a graph edge
//! key       key_len bytes
//! value     value_len bytes
//! padding   to a multiple of 8
//! ```
//!
//! Three fields are doing more than their name says.
//!
//! `previous` is both the version chain and the collision chain. A new
//! version of a key points at the old one, and a record that takes over
//! a full index entry points at whatever that entry held, so a walk of
//! the chain may pass records belonging to other keys. That is why
//! every step compares the key rather than trusting the link.
//!
//! `version` carries the in-place lock in its top bit, which makes the
//! record a seqlock: a reader takes the version, copies the value,
//! takes the version again, and retries if it moved or was locked; an
//! in-place writer sets the bit, writes, and publishes a new version
//! with the bit clear. This is the optimistic validation of Leis,
//! Scheibner, Kemper and Neumann (DaMoN 2016) at record granularity,
//! and it is what lets the mutable region take an update without any
//! reader holding a latch. Commit sequence numbers are 63 bits, which
//! at a billion commits a second lasts 292 years.
//!
//! `kind` is what keeps edges out of the hash index. An edge is not a
//! keyed record, because a version cell per edge is what makes dynamic
//! graph stores cost 4 to 8x a static CSR (arXiv 2502.10959, 2025). It
//! still has to be durable, so it goes on the same log with a kind that
//! tells recovery to replay it into the adjacency rather than install
//! it as a key.
//!
//! `crc` is what makes recovery precise rather than heuristic. A
//! forward scan stops at the first record whose checksum does not hold,
//! and that record is the boundary of the durable prefix. It is
//! computed with the lock bit cleared, so a record that was rewritten
//! in place still checks out, and it is recomputed under the lock when
//! that happens. Nothing on the read path looks at it: a point read
//! that verified a checksum would pay the scan cost on every operation
//! for a guarantee the flush already made.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::addr::Address;

/// Bytes of header before the key.
pub const HEADER: usize = 32;

/// Top bit of `version`: a writer is rewriting the value in place.
pub const LOCK: u64 = 1 << 63;

/// Top bit of `key_len`: the record is a delete.
pub const TOMBSTONE: u32 = 1 << 31;

/// The longest key, since the top bit of `key_len` is spoken for.
pub const MAX_KEY: usize = (TOMBSTONE - 1) as usize;

/// A keyed value, which the index names.
pub const KIND_VALUE: u32 = 0;

/// An adjacency change, which recovery replays into the graph plane and
/// the index never sees.
pub const KIND_EDGE: u32 = 1;

/// A keyed value that is also a node's external key, whose value is
/// the four bytes of its dense id. The index treats it like any other
/// value; the graph plane needs it so that recovery can restore the id
/// counter without a second record per node.
pub const KIND_VERTEX: u32 = 2;

/// The rest of this page is padding, written where the allocator
/// skipped to the next page because a record did not fit in what was
/// left.
///
/// It exists so that zeros stop meaning two things. A page's leftover
/// bytes are zeros and so is a block the device lost, and recovery read
/// both as padding, so a hole in the middle of a page cost the rest of
/// that page and then the scan carried on above it. What survived was a
/// prefix with a suffix stapled on, which is exactly what the log's
/// durability contract says cannot happen (#472).
///
/// A pad is a bare header. It does not say how long the gap is because
/// it does not need to: a pad always means the rest of the page, and a
/// length field would be a second thing to get wrong. What makes it
/// tell itself apart from zeros is that it carries a real checksum,
/// which an all-zero header does not.
pub const KIND_PAD: u32 = 3;

/// Bytes a record with these lengths occupies, padded so the next
/// record starts 8 byte aligned.
#[inline]
pub const fn size_of(key_len: usize, value_len: usize) -> usize {
    (HEADER + key_len + value_len).next_multiple_of(8)
}

/// The checksum of a record's header words and byte runs.
fn checksum(previous: u64, version: u64, lengths: u64, key: &[u8], value: &[u8]) -> u32 {
    let mut crc = crc32c::crc32c(&previous.to_le_bytes());
    crc = crc32c::crc32c_append(crc, &(version & !LOCK).to_le_bytes());
    crc = crc32c::crc32c_append(crc, &lengths.to_le_bytes());
    crc = crc32c::crc32c_append(crc, key);
    crc32c::crc32c_append(crc, value)
}

/// A record in place, either in a log page or in an aligned buffer read
/// off disk.
///
/// # Safety
///
/// The pointer must be 8 byte aligned and point at a whole record whose
/// bytes stay put and stay mapped for `'a`. In the log that holds
/// because a record is written before its address is published and the
/// page it sits in is held by the reader's epoch; in a read buffer it
/// holds because the buffer outlives the view.
#[derive(Clone, Copy)]
pub struct RecordRef<'a> {
    base: *const u8,
    life: std::marker::PhantomData<&'a [u8]>,
}

impl<'a> RecordRef<'a> {
    /// # Safety
    ///
    /// See the type's contract.
    #[inline]
    pub const unsafe fn new(base: *const u8) -> Self {
        Self {
            base,
            life: std::marker::PhantomData,
        }
    }

    #[inline]
    fn word(&self, index: usize) -> u64 {
        // SAFETY: the contract puts a whole 32 byte header at base and
        // guarantees 8 byte alignment, so all four words are readable.
        unsafe { self.base.cast::<u64>().add(index).read() }
    }

    /// The address of the prior version, or of whatever record this one
    /// displaced from a full index entry.
    #[inline]
    pub fn previous(&self) -> Address {
        self.word(0)
    }

    /// The version cell, for the seqlock protocol.
    #[inline]
    pub fn version_cell(&self) -> &'a AtomicU64 {
        // SAFETY: the second header word is an AtomicU64 written only
        // through this accessor, and AtomicU64 has the layout of u64.
        unsafe { &*self.base.add(8).cast::<AtomicU64>() }
    }

    /// The commit sequence number, with the lock bit masked off.
    #[inline]
    pub fn version(&self) -> u64 {
        self.version_cell().load(Ordering::Acquire) & !LOCK
    }

    #[inline]
    fn lengths(&self) -> u64 {
        self.word(2)
    }

    /// Whether this record is a delete.
    #[inline]
    pub fn tombstone(&self) -> bool {
        self.lengths() as u32 & TOMBSTONE != 0
    }

    #[inline]
    pub fn key_len(&self) -> usize {
        (self.lengths() as u32 & !TOMBSTONE) as usize
    }

    #[inline]
    pub fn value_len(&self) -> usize {
        (self.lengths() >> 32) as usize
    }

    /// What the record is: [`KIND_VALUE`] or [`KIND_EDGE`].
    #[inline]
    pub fn kind(&self) -> u32 {
        (self.word(3) >> 32) as u32
    }

    /// Bytes this record occupies on the log.
    #[inline]
    pub fn size(&self) -> usize {
        size_of(self.key_len(), self.value_len())
    }

    /// The key. Immutable for the life of the record, so no validation
    /// is needed to read it.
    #[inline]
    pub fn key(&self) -> &'a [u8] {
        // SAFETY: the contract covers HEADER + key_len bytes.
        unsafe { std::slice::from_raw_parts(self.base.add(HEADER), self.key_len()) }
    }

    /// The value bytes without seqlock validation. Correct for records
    /// below the read-only boundary, which never change, and for the
    /// writer that holds the lock. Everywhere else use
    /// [`RecordRef::read_value`].
    ///
    /// # Safety
    ///
    /// The caller asserts no concurrent in-place writer.
    #[inline]
    pub unsafe fn value_unchecked(&self) -> &'a [u8] {
        // SAFETY: the contract covers the whole record.
        unsafe {
            std::slice::from_raw_parts(self.base.add(HEADER + self.key_len()), self.value_len())
        }
    }

    /// Copies the value out under the seqlock, so a concurrent in-place
    /// update is either wholly seen or wholly missed. Returns the
    /// version the value was consistent at.
    pub fn read_value(&self, out: &mut Vec<u8>) -> u64 {
        loop {
            let before = self.version_cell().load(Ordering::Acquire);
            if before & LOCK != 0 {
                std::hint::spin_loop();
                continue;
            }
            out.clear();
            // SAFETY: same bytes as value_unchecked; a racing writer is
            // caught by the version recheck below, and the length can
            // not change in place because only the value may.
            out.extend_from_slice(unsafe { self.value_unchecked() });
            if self.version_cell().load(Ordering::Acquire) == before {
                return before;
            }
        }
    }

    /// Rewrites the value in place under the lock, refreshes the
    /// checksum, and publishes `version`. Returns false when the record
    /// is already locked, which the caller answers by appending.
    ///
    /// # Safety
    ///
    /// The record must be at or above the read-only boundary, so that
    /// no reader is entitled to assume it is immutable, and `value`
    /// must be exactly as long as the value already there.
    pub unsafe fn write_value_in_place(&self, value: &[u8], version: u64) -> bool {
        debug_assert_eq!(value.len(), self.value_len());
        let cell = self.version_cell();
        let current = cell.load(Ordering::Acquire);
        if current & LOCK != 0
            || cell
                .compare_exchange(current, current | LOCK, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        // SAFETY: the lock bit is ours, so no other writer is here, and
        // readers validate the version after copying.
        unsafe {
            let dst = self.base.add(HEADER + self.key_len()).cast_mut();
            std::ptr::copy_nonoverlapping(value.as_ptr(), dst, value.len());
            let crc = checksum(
                self.previous(),
                version,
                self.lengths(),
                self.key(),
                self.value_unchecked(),
            );
            self.base.add(24).cast::<u32>().cast_mut().write(crc);
        }
        cell.store(version, Ordering::Release);
        true
    }

    /// Points the record at a different prior record and refreshes the
    /// checksum, so a scan that reads it later still accepts it.
    ///
    /// Recovery only. The live path never moves a link: a chain is only
    /// ever extended at its head, by a record whose address nobody has
    /// seen yet, which is what lets a reader walk one without a latch.
    /// Recovery has no readers to race and a reason the live path does
    /// not have, which [`crate::recover::install`] explains.
    ///
    /// # Safety
    ///
    /// The caller asserts it is the only thread that can see this
    /// record, and that the record is in a page it can write back.
    pub unsafe fn relink(&self, previous: Address) {
        // SAFETY: the contract covers the whole record, and the caller
        // has said nothing else is looking at it.
        unsafe {
            self.base.cast::<u64>().cast_mut().write(previous);
            let crc = checksum(
                previous,
                self.version_cell().load(Ordering::Relaxed),
                self.lengths(),
                self.key(),
                self.value_unchecked(),
            );
            self.base.add(24).cast::<u32>().cast_mut().write(crc);
        }
    }

    /// Whether the record's checksum holds. Recovery asks; the read
    /// path does not.
    pub fn intact(&self) -> bool {
        // SAFETY: recovery calls this on a record whose lengths it has
        // already bounded against the page, and no writer is running.
        let value = unsafe { self.value_unchecked() };
        let stored = self.word(3) as u32;
        stored
            == checksum(
                self.previous(),
                self.version_cell().load(Ordering::Relaxed),
                self.lengths(),
                self.key(),
                value,
            )
    }
}

/// Lays a record down at `dst`, which must have room for [`size_of`]
/// bytes and be 8 byte aligned.
///
/// # Safety
///
/// The caller owns the destination range: in the log that means the
/// range came back from the tail allocator and its address has not been
/// published.
pub unsafe fn write_at(
    dst: *mut u8,
    previous: Address,
    version: u64,
    key: &[u8],
    value: &[u8],
    tombstone: bool,
    kind: u32,
) {
    debug_assert_eq!(dst as usize % 8, 0);
    let mut key_field = key.len() as u32;
    if tombstone {
        key_field |= TOMBSTONE;
    }
    let lengths = u64::from(key_field) | (u64::from(value.len() as u32) << 32);
    let crc = checksum(previous, version, lengths, key, value);
    // SAFETY: the caller owns size_of(key, value) bytes at dst, and the
    // four header words plus both byte runs fit inside that.
    unsafe {
        dst.cast::<u64>().write(previous);
        dst.cast::<u64>().add(1).write(version);
        dst.cast::<u64>().add(2).write(lengths);
        dst.cast::<u64>()
            .add(3)
            .write(u64::from(crc) | u64::from(kind) << 32);
        std::ptr::copy_nonoverlapping(key.as_ptr(), dst.add(HEADER), key.len());
        std::ptr::copy_nonoverlapping(value.as_ptr(), dst.add(HEADER + key.len()), value.len());
        // The padding is written so that a recovery scan over a page
        // that was reused can not mistake stale bytes for a record.
        let used = HEADER + key.len() + value.len();
        let padded = size_of(key.len(), value.len());
        std::ptr::write_bytes(dst.add(used), 0, padded - used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8 byte aligned scratch buffer, the shape the disk read path
    /// uses so that a record parsed out of a buffer parses the same way
    /// as one parsed out of a page.
    fn buffer(bytes: usize) -> Vec<u64> {
        vec![0u64; bytes.div_ceil(8)]
    }

    #[test]
    fn a_record_round_trips_through_its_bytes() {
        let key = b"user1234567890";
        let value = b"field0=abcdef,field1=012345";
        let mut buf = buffer(size_of(key.len(), value.len()));
        // SAFETY: the buffer is 8 byte aligned and long enough.
        let r = unsafe {
            write_at(
                buf.as_mut_ptr().cast(),
                4096,
                7,
                key,
                value,
                false,
                KIND_VALUE,
            );
            RecordRef::new(buf.as_ptr().cast())
        };
        assert_eq!(r.previous(), 4096);
        assert_eq!(r.version(), 7);
        assert_eq!(r.key(), key);
        // SAFETY: nothing else touches this record.
        assert_eq!(unsafe { r.value_unchecked() }, value);
        assert!(!r.tombstone());
        assert_eq!(r.kind(), KIND_VALUE);
        assert!(r.intact());
        assert_eq!(r.size(), size_of(key.len(), value.len()));
        assert_eq!(r.size() % 8, 0);
    }

    #[test]
    fn a_tombstone_keeps_its_key_length() {
        let key = b"gone";
        let mut buf = buffer(size_of(key.len(), 0));
        // SAFETY: as above.
        let r = unsafe {
            write_at(buf.as_mut_ptr().cast(), 0, 9, key, &[], true, KIND_VALUE);
            RecordRef::new(buf.as_ptr().cast())
        };
        assert!(r.tombstone());
        assert_eq!(r.key(), key);
        assert_eq!(r.value_len(), 0);
        assert!(r.intact());
    }

    #[test]
    fn an_in_place_write_is_seen_whole_and_keeps_the_checksum_true() {
        let key = b"k";
        let mut buf = buffer(size_of(key.len(), 8));
        // SAFETY: as above, and the record is not shared.
        let r = unsafe {
            write_at(
                buf.as_mut_ptr().cast(),
                0,
                1,
                key,
                &[0u8; 8],
                false,
                KIND_VALUE,
            );
            RecordRef::new(buf.as_ptr().cast())
        };
        let mut out = Vec::new();
        assert_eq!(r.read_value(&mut out), 1);
        assert_eq!(out, vec![0u8; 8]);
        // SAFETY: the test is the only thread, and the length matches.
        assert!(unsafe { r.write_value_in_place(&[9u8; 8], 2) });
        assert_eq!(r.read_value(&mut out), 2);
        assert_eq!(out, vec![9u8; 8]);
        assert_eq!(r.version(), 2);
        assert!(r.intact());
    }

    #[test]
    fn a_flipped_byte_fails_the_checksum() {
        let key = b"key";
        let value = b"value";
        let mut buf = buffer(size_of(key.len(), value.len()));
        // SAFETY: as above.
        unsafe { write_at(buf.as_mut_ptr().cast(), 0, 3, key, value, false, KIND_VALUE) };
        let bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast(), size_of(key.len(), value.len()))
        };
        bytes[HEADER + key.len()] ^= 0x40;
        // SAFETY: still a whole record, just a wrong one.
        let r = unsafe { RecordRef::new(buf.as_ptr().cast()) };
        assert!(!r.intact());
    }
}
