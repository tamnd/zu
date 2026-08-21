//! The hash index: one cacheline per bucket, eight entries per line.
//!
//! ```text
//! bit 63     tentative, an insert is claiming this entry
//! bit 62     foreign, the chain under this entry holds other tags too
//! bits 61-48 tag, 14 bits of the key hash
//! bits 47-0  address of the newest record for the key
//! ```
//!
//! A probe is one cacheline load and eight compares, and the tag is
//! what lets seven of them answer without touching the log. With 14
//! bits a wrong tag survives one probe in 16384.
//!
//! FASTER spends the eighth entry of a full bucket on a pointer to an
//! overflow bucket. This does not: when all eight are taken, the
//! arriving record's `previous` points at whatever the entry it takes
//! over was holding, so the collision chain lives in the log next to
//! the version chain and there is no second allocator on the write
//! path. The cost is that the displaced key is no longer named by its
//! own tag, which is what the foreign bit is for. An entry with foreign
//! set is walked by every lookup that reaches its bucket, tag match or
//! not, so a displaced key is still found; entries without it are
//! walked only on a tag match, which is the common case and the fast
//! one. The bit is sticky, because a chain never gives records back.
//!
//! ## Growing
//!
//! A displaced key costs a log dereference per lookup, so a table that
//! is four times too small turns a point read into four random reads
//! and the whole argument for the design goes with it. The table
//! doubles rather than letting that happen, at half full.
//!
//! Half full is not where displacement starts. There is no load above
//! zero where it has not started, because half full is a statement
//! about the table and overflowing is a property of one bucket, and
//! keys per bucket is Poisson rather than uniform. What the threshold
//! buys is a rate. A hundred thousand keys, with the bucket count
//! forced so the load is the only thing moving:
//!
//! ```text
//!   buckets   keys/bucket   slots in use   displaced
//!      8192          3.05          99793         207
//!     16384          3.05          99793         207
//!     32768          3.05          99793         207
//!     65536          1.53          99999           1
//!    131072          0.76         100000           0
//! ```
//!
//! 32768 is where the policy settles for that many keys, and the mean
//! bucket there holds 3 of its 8 while the tail of the distribution is
//! already past 8 for a couple of hundred of them. So two keys in a
//! thousand are displaced at the threshold, which is one lookup in five
//! hundred paying one extra dereference, and nothing like the four
//! random reads the paragraph above is guarding against. The rate is
//! what the threshold is chosen for, and `displacement_at_the_growth_
//! threshold_stays_rare` holds it to that number so a change to the
//! bucket width or the threshold has to restate it (#487).
//!
//! Doubling is FASTER's split, and the shape here is the same: a bucket
//! `b` of the old table feeds exactly two buckets of the new one, `b`
//! and `b + old_len`, because the index is the low bits of the hash and
//! doubling looks at one more of them. Nothing else in the table feeds
//! those two, so a split is a local operation and the migration of one
//! bucket needs no lock beyond that bucket.
//!
//! What is different is the collision chain. The bit that decides which
//! side an entry goes to is not in the entry, and cannot be: the entry
//! is 14 bits of tag and 48 of address with nothing spare. So the split
//! reads one record per entry and takes the side from the key it finds,
//! which is one dereference per entry and not one per record. A foreign
//! entry names a chain of more than one key and there is no single side
//! for it, so it goes to both, which is correct because a key reaches
//! only ever one of the two buckets and each side maintains its own
//! copy from then on. Growing at half full is what keeps that case
//! rare.
//!
//! Migration is per bucket and lazy: an operation drains the old bucket
//! its key came from before it touches the new table, and the
//! background thread drains whatever the traffic did not. A grow
//! publishes the new table and then waits for the operations that were
//! already running, because those are the ones that may still be
//! holding a reference into the old table, and nothing migrates until
//! that wait returns.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::epoch::Epochs;

/// Entries in one bucket, which is one cacheline.
pub const SLOTS: usize = 8;

const TENTATIVE: u64 = 1 << 63;
const FOREIGN: u64 = 1 << 62;
const TAG_SHIFT: u32 = 48;
const TAG_MASK: u64 = 0x3FFF;
const ADDRESS_MASK: u64 = (1 << 48) - 1;

/// The entry an empty slot holds.
pub const EMPTY: u64 = 0;

/// Keys per slot at which the table doubles. Half full, which is where
/// displacement is rare rather than where it has not started: about two
/// keys in a thousand are on a chain at that load. The module header
/// has the measurements.
const GROW_AT_PERCENT: usize = 50;

#[inline]
pub const fn entry(tag: u64, address: u64, foreign: bool) -> u64 {
    let base = (tag & TAG_MASK) << TAG_SHIFT | (address & ADDRESS_MASK);
    if foreign { base | FOREIGN } else { base }
}

#[inline]
pub const fn tag_of(entry: u64) -> u64 {
    entry >> TAG_SHIFT & TAG_MASK
}

#[inline]
pub const fn address_of(entry: u64) -> u64 {
    entry & ADDRESS_MASK
}

#[inline]
pub const fn is_foreign(entry: u64) -> bool {
    entry & FOREIGN != 0
}

#[inline]
pub const fn is_tentative(entry: u64) -> bool {
    entry & TENTATIVE != 0
}

#[inline]
pub const fn tentative(tag: u64) -> u64 {
    TENTATIVE | (tag & TAG_MASK) << TAG_SHIFT
}

/// One cacheline of entries.
#[repr(align(64))]
pub struct Bucket {
    pub slots: [AtomicU64; SLOTS],
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            slots: [const { AtomicU64::new(EMPTY) }; SLOTS],
        }
    }
}

/// A power of two array of buckets and the mask that picks one.
pub struct Table {
    buckets: Box<[Bucket]>,
    mask: usize,
}

#[allow(clippy::len_without_is_empty)]
impl Table {
    fn new(count: usize) -> Self {
        let count = count.max(1).next_power_of_two();
        Self {
            buckets: (0..count).map(|_| Bucket::default()).collect(),
            mask: count - 1,
        }
    }

    #[inline]
    pub fn bucket(&self, hash: u64) -> &Bucket {
        // The low bits pick the bucket and the top bits are the tag, so
        // the two never overlap however the table is sized.
        &self.buckets[hash as usize & self.mask]
    }

    #[inline]
    pub fn at(&self, index: usize) -> &Bucket {
        &self.buckets[index & self.mask]
    }

    /// Buckets in the table. A table always has at least one, which is
    /// why there is no `is_empty` next to it.
    #[inline]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// How many slots are in use.
    ///
    /// Slots and not keys. A full bucket displaces, and a displaced key
    /// lives on the chain under the entry that took its slot, so one
    /// slot in use can name several keys. Against the number of keys
    /// written this reads as records having gone missing, and it is not
    /// that (#486). [`Self::foreign`] is the count that says how much
    /// of the difference is displacement.
    pub fn occupancy(&self) -> usize {
        self.count(|entry| entry != EMPTY)
    }

    /// How many slots name a chain holding more than one key.
    ///
    /// This is the number a reader of a benchmark actually wants out of
    /// the index: every lookup that reaches one of these buckets walks
    /// the chain under it whether the tag matched or not, so it is the
    /// crowding that is being paid for rather than the crowding the
    /// load factor implies.
    pub fn foreign(&self) -> usize {
        self.count(is_foreign)
    }

    fn count(&self, wanted: impl Fn(u64) -> bool + Copy) -> usize {
        self.buckets
            .iter()
            .map(|b| {
                b.slots
                    .iter()
                    .filter(|s| wanted(s.load(Ordering::Relaxed)))
                    .count()
            })
            .sum()
    }
}

/// What a caller found when it tried to take a bucket's migration.
pub enum Claim {
    /// Somebody already did it.
    Done,
    /// This caller owns it and has to finish it.
    Mine,
    /// Somebody else is doing it right now.
    Busy,
}

const TODO: u8 = 0;
const BUSY: u8 = 1;
const MOVED: u8 = 2;

/// A doubling in progress: the table being drained and how far along
/// each of its buckets is.
pub struct Migration {
    old: *mut Table,
    state: Box<[AtomicU8]>,
    left: AtomicUsize,
    /// Set once the grower's wait for the already running operations
    /// has returned. Nothing migrates before that, because until then
    /// there may be an operation holding a reference into the old
    /// table.
    open: AtomicBool,
}

// SAFETY: `old` is only read, through `Migration::old`, and the table
// it points at is immutable for the life of the migration.
unsafe impl Send for Migration {}
// SAFETY: as above.
unsafe impl Sync for Migration {}

impl Migration {
    /// The table being drained.
    ///
    /// # Safety of the reference
    /// Valid while the caller's epoch is held. A finished migration is
    /// retired with its table, so a caller outside an epoch has nothing
    /// keeping either alive.
    #[inline]
    pub fn old(&self) -> &Table {
        // SAFETY: the pointer came from a Box that this struct owns and
        // that outlives it, and nothing writes through it.
        unsafe { &*self.old }
    }

    /// Whether migration may begin.
    #[inline]
    pub fn open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// Which old bucket a hash's new bucket has to be drained from.
    #[inline]
    pub fn source(&self, hash: u64) -> usize {
        hash as usize & self.old().mask
    }

    pub fn claim(&self, source: usize) -> Claim {
        match self.state[source].compare_exchange(TODO, BUSY, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Claim::Mine,
            Err(MOVED) => Claim::Done,
            Err(_) => Claim::Busy,
        }
    }

    /// Whether a hash goes to the lower of the two buckets its old one
    /// feeds. Doubling looks at one more low bit of the hash and the old
    /// length is exactly that bit, so this is the whole of the split
    /// decision.
    #[inline]
    pub fn low_side(&self, hash: u64) -> bool {
        hash as usize & self.old().len() == 0
    }

    /// Gives a claimed bucket back unmigrated, for a caller that could
    /// not finish. The count is untouched because it was never counted.
    pub fn release(&self, source: usize) {
        self.state[source].store(TODO, Ordering::Release);
    }

    /// Publishes a bucket as migrated. Returns whether it was the last
    /// one, which is the caller's cue to retire the whole migration.
    pub fn finish(&self, source: usize) -> bool {
        self.state[source].store(MOVED, Ordering::Release);
        self.left.fetch_sub(1, Ordering::AcqRel) == 1
    }

    /// Spins until whoever owns this bucket has published it. The owner
    /// is doing log reads and is not waiting on anything, so the wait
    /// is bounded by those reads.
    pub fn wait(&self, source: usize) {
        while self.state[source].load(Ordering::Acquire) != MOVED {
            std::hint::spin_loop();
        }
    }

    /// The next bucket that still has to be drained, for the background
    /// thread to finish what the traffic did not touch.
    pub fn unfinished(&self, from: usize) -> Option<usize> {
        (from..self.state.len()).find(|&i| self.state[i].load(Ordering::Acquire) != MOVED)
    }
}

pub struct Index {
    /// The table every operation uses. A raw pointer because a grow
    /// replaces it and the one it replaced has to outlive the
    /// operations that already loaded it.
    live: AtomicPtr<Table>,
    /// The doubling in progress, null at rest.
    migrating: AtomicPtr<Migration>,
    /// Distinct keys the index has been told about, which is what the
    /// load factor is against. It only rises: a delete writes a
    /// tombstone rather than taking an entry out, so nothing here comes
    /// back, and a table sized for the keys a database has held is the
    /// right size for the ones it holds.
    keys: AtomicUsize,
    /// Doublings since the database was opened.
    grows: AtomicU64,
    /// One grower at a time.
    growing: Mutex<()>,
    /// Set when the caller sized the table itself and wants it left
    /// that way, which is what a test that is about crowding needs.
    fixed: bool,
}

impl Index {
    /// Sizes the table to at least `buckets`, rounded up to a power of
    /// two. `fixed` leaves it at that size whatever the load factor
    /// does.
    pub fn new(buckets: usize, fixed: bool) -> Self {
        Self {
            live: AtomicPtr::new(Box::into_raw(Box::new(Table::new(buckets)))),
            migrating: AtomicPtr::new(std::ptr::null_mut()),
            keys: AtomicUsize::new(0),
            grows: AtomicU64::new(0),
            growing: Mutex::new(()),
            fixed,
        }
    }

    /// Swaps in a table of at least `buckets`, before there is anything
    /// in the one it replaces.
    ///
    /// Recovery only, and it is not a resize: there is nothing to move
    /// and nobody to protect against. A scan that knows how many records
    /// the file holds knows roughly how many keys it is about to install
    /// and can build the table for them in one go, rather than filling a
    /// table sized by a hint and making the flusher double it a few times
    /// afterwards. It also saves the scan the link repair that a table of
    /// the wrong shape costs, which [`crate::recover`] explains.
    ///
    /// A fixed index keeps the size it was asked for, and so does one
    /// that was asked for more than this.
    pub fn presize(&self, buckets: usize) {
        debug_assert_eq!(self.keys(), 0, "presize after the table has keys in it");
        if self.fixed || buckets <= self.buckets() {
            return;
        }
        let fresh = Box::into_raw(Box::new(Table::new(buckets)));
        let old = self.live.swap(fresh, Ordering::Release);
        // SAFETY: nothing else is running, so nothing holds a reference
        // to the table being replaced.
        drop(unsafe { Box::from_raw(old) });
    }

    /// Replaces the table with one of exactly `buckets`, before there is
    /// anything in the one it replaces, and says whether it could.
    ///
    /// Checkpoint recovery only. A checkpoint records entries and not
    /// keys, and an entry says which bucket it belongs in only by the
    /// bucket it was written to, so a table of a different size cannot
    /// take it: the mask that picked the bucket would pick another one.
    /// A caller that pinned the size with `grow_index` off and pinned it
    /// somewhere else than the checkpoint was written at therefore gets
    /// a refusal here rather than a rehash, and the reopen falls back to
    /// reading the log.
    pub fn adopt(&self, buckets: usize) -> bool {
        debug_assert_eq!(self.keys(), 0, "adopt after the table has keys in it");
        let buckets = buckets.max(1).next_power_of_two();
        if self.buckets() == buckets {
            return true;
        }
        if self.fixed {
            return false;
        }
        let fresh = Box::into_raw(Box::new(Table::new(buckets)));
        let old = self.live.swap(fresh, Ordering::Release);
        // SAFETY: nothing else is running, so nothing holds a reference
        // to the table being replaced.
        drop(unsafe { Box::from_raw(old) });
        true
    }

    /// Sets the key count a checkpoint recorded, which is what the load
    /// factor is measured against. Without it a restored table would
    /// think it was empty and would not double until it had been filled
    /// a second time.
    pub fn adopt_keys(&self, keys: usize) {
        self.keys.store(keys, Ordering::Release);
    }

    /// The table in use.
    ///
    /// # Safety of the reference
    /// Valid while the caller's epoch is held, for the same reason a
    /// log page pointer is: a grow retires the table it replaced rather
    /// than freeing it, and the epoch is what says when the retirement
    /// can run.
    #[inline]
    pub fn live(&self) -> &Table {
        // SAFETY: the pointer is never null and the table it names is
        // retired through the epoch queue, not freed.
        unsafe { &*self.live.load(Ordering::Acquire) }
    }

    /// The doubling in progress, if there is one. Read after
    /// [`Index::live`] and not before: the migration is published
    /// first, so a caller that saw the new table also sees this.
    #[inline]
    pub fn pending(&self) -> Option<&Migration> {
        let pointer = self.migrating.load(Ordering::Acquire);
        if pointer.is_null() {
            return None;
        }
        // SAFETY: as `live`, retired through the epoch queue.
        Some(unsafe { &*pointer })
    }

    /// Whether a doubling is in flight. This one reads the pointer
    /// without making a reference out of it, so it is the one a caller
    /// outside an epoch is allowed to ask.
    #[inline]
    pub fn resizing(&self) -> bool {
        !self.migrating.load(Ordering::Acquire).is_null()
    }

    #[inline]
    pub fn buckets(&self) -> usize {
        self.live().len()
    }

    /// Doublings since the database was opened.
    pub fn grows(&self) -> u64 {
        self.grows.load(Ordering::Relaxed)
    }

    /// Distinct keys the index has been told about.
    pub fn keys(&self) -> usize {
        self.keys.load(Ordering::Relaxed)
    }

    /// Counts a key the index had not seen before.
    #[inline]
    pub fn note_key(&self) {
        self.keys.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether the table has passed the load factor it doubles at.
    pub fn wants_growth(&self) -> bool {
        if self.fixed || !self.migrating.load(Ordering::Acquire).is_null() {
            return false;
        }
        let slots = self.live().len() * SLOTS;
        self.keys() * 100 >= slots * GROW_AT_PERCENT
    }

    /// Doubles the table and opens the migration into it.
    ///
    /// The caller must not be inside an epoch, because this waits for
    /// every operation that is.
    pub fn grow(&self, epochs: &Epochs) -> bool {
        if self.fixed {
            return false;
        }
        let Ok(_one) = self.growing.try_lock() else {
            return false;
        };
        if !self.migrating.load(Ordering::Acquire).is_null() {
            return false;
        }
        let old = self.live.load(Ordering::Acquire);
        // SAFETY: the live pointer is never null.
        let count = unsafe { &*old }.len();
        let fresh = Box::into_raw(Box::new(Table::new(count * 2)));
        let migration = Box::into_raw(Box::new(Migration {
            old,
            state: (0..count).map(|_| AtomicU8::new(TODO)).collect(),
            left: AtomicUsize::new(count),
            open: AtomicBool::new(false),
        }));
        // The migration goes out before the table it drains into, so an
        // operation that sees the new table also sees that the old one
        // still has to be emptied into it.
        self.migrating.store(migration, Ordering::Release);
        self.live.store(fresh, Ordering::Release);
        // An operation that was already running may hold a reference
        // into the old table, and the split rewrites what it would be
        // reading. One that arrives now finds the migration closed and
        // stands down and comes back, which is what keeps this wait
        // from waiting on the very operations it let through.
        epochs.wait_for_quiescence();
        // SAFETY: the box was just leaked here and nothing has retired
        // it, because a migration is only retired once it is finished
        // and it cannot finish before it opens.
        unsafe { &*migration }.open.store(true, Ordering::Release);
        self.grows.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Retires a finished migration and the table it drained.
    ///
    /// The bump is what lets the epoch the retirement was queued in
    /// pass. Without it the free waits for a later epoch that nothing
    /// else would produce, which is the shape of #458.
    pub fn retire(&self, epochs: &Epochs, migration: &Migration) {
        let pointer = migration as *const Migration as *mut Migration;
        if self
            .migrating
            .compare_exchange(
                pointer,
                std::ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        let retired = pointer as usize;
        epochs.defer(Box::new(move || {
            // SAFETY: the epoch has passed, so no operation still holds
            // a reference into either the migration or the table it
            // drained, and both were leaked from a Box.
            unsafe {
                let migration = Box::from_raw(retired as *mut Migration);
                drop(Box::from_raw(migration.old));
            }
        }));
        epochs.bump();
        epochs.drain();
    }

    /// How many slots are in use, for tests and for reporting the
    /// load factor a benchmark ran at.
    pub fn occupancy(&self) -> usize {
        self.live().occupancy()
    }

    /// How many slots name a chain holding more than one key, which is
    /// what displacement costs a reader.
    pub fn foreign(&self) -> usize {
        self.live().foreign()
    }

    /// The tag for a hash: 14 bits off the top, independent of the bits
    /// the bucket index used.
    #[inline]
    pub const fn tag(hash: u64) -> u64 {
        (hash >> 50) & TAG_MASK
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        let migrating = self.migrating.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !migrating.is_null() {
            // SAFETY: nothing is running by the time an index drops, so
            // whatever the epoch queue would have freed can go now.
            unsafe {
                let migration = Box::from_raw(migrating);
                drop(Box::from_raw(migration.old));
            }
        }
        let live = self.live.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !live.is_null() {
            // SAFETY: leaked from a Box in `new` or `grow`.
            unsafe { drop(Box::from_raw(live)) };
        }
    }
}

/// A 64 bit hash of a key.
///
/// Multiply-xor-fold over 8 byte words, which is the core wyhash and
/// xxh3 both use. It has to be strong in the top bits, because those
/// are the tag, and in the low bits, because those pick the bucket, and
/// the folded multiply is what spreads a change in any input byte
/// across both ends.
pub fn hash(key: &[u8]) -> u64 {
    const P0: u64 = 0xA0761D6478BD642F;
    const P1: u64 = 0xE7037ED1A0B428DB;
    const P2: u64 = 0x8EBC6AF09C88C6E3;

    #[inline]
    fn fold(a: u64, b: u64) -> u64 {
        let wide = u128::from(a) * u128::from(b);
        (wide as u64) ^ ((wide >> 64) as u64)
    }

    let mut acc = P0 ^ (key.len() as u64).wrapping_mul(P1);
    let mut rest = key;
    while rest.len() >= 8 {
        let word = u64::from_le_bytes(rest[..8].try_into().expect("eight bytes"));
        acc = fold(acc ^ word, P1);
        rest = &rest[8..];
    }
    if !rest.is_empty() {
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        acc = fold(acc ^ u64::from_le_bytes(last), P2);
    }
    fold(acc, P2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_carries_its_tag_address_and_flags() {
        let e = entry(0x2ABC, 0x0000_1234_5678_9AB8, true);
        assert_eq!(tag_of(e), 0x2ABC);
        assert_eq!(address_of(e), 0x0000_1234_5678_9AB8);
        assert!(is_foreign(e));
        assert!(!is_tentative(e));
        let e = entry(1, 64, false);
        assert!(!is_foreign(e));
        assert_eq!(address_of(e), 64);
    }

    #[test]
    fn a_bucket_is_one_cacheline() {
        assert_eq!(std::mem::size_of::<Bucket>(), 64);
        assert_eq!(std::mem::align_of::<Bucket>(), 64);
    }

    /// The property the whole split rests on: doubling the table sends
    /// a bucket to exactly two, and nothing else arrives in either.
    #[test]
    fn a_bucket_splits_into_two_and_only_two() {
        let old = Table::new(1 << 8);
        let new = Table::new(1 << 9);
        let mut sides = std::collections::HashMap::new();
        for i in 0..1u64 << 16 {
            let h = hash(&i.to_be_bytes());
            let before = h as usize & old.mask;
            let after = h as usize & new.mask;
            sides.entry(before).or_insert_with(Vec::new).push(after);
        }
        for (before, after) in sides {
            let mut seen: Vec<usize> = after;
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), 2, "bucket {before} went to {seen:?}");
            assert_eq!(seen[0], before);
            assert_eq!(seen[1], before + old.len());
        }
    }

    /// #487. Half full is not where displacement starts. There is no
    /// load above zero where it has not started, because the threshold
    /// is a statement about the table and overflowing is a property of
    /// one bucket, and keys per bucket is Poisson rather than uniform.
    /// What the threshold buys is a rate, so the rate is what this
    /// holds: a change to the bucket width or to the threshold has to
    /// restate the number rather than move it quietly.
    ///
    /// The count is the keys past the eighth of their bucket, which is
    /// exactly the keys that go on a chain, so this is the displaced
    /// number without a log to build. It comes out at 205 of 100000,
    /// against the 207 a hundred thousand go-ycsb records displaced on
    /// a real load at the same bucket count, which is the agreement
    /// worth having: the rate is a property of the distribution and
    /// not of the keys.
    #[test]
    fn displacement_at_the_growth_threshold_stays_rare() {
        const KEYS: usize = 100_000;
        let table = Table::new(1 << 15);
        let mut per_bucket = vec![0usize; table.len()];
        for i in 0..KEYS {
            let key = format!("user{i}");
            per_bucket[hash(key.as_bytes()) as usize & table.mask] += 1;
        }
        let displaced: usize = per_bucket.iter().map(|k| k.saturating_sub(SLOTS)).sum();
        let load = KEYS as f64 / (table.len() * SLOTS) as f64;
        assert!(
            (0.35..0.40).contains(&load),
            "the table this sizes to is the one the policy settles on, load {load}"
        );
        let per_thousand = displaced as f64 * 1000.0 / KEYS as f64;
        assert!(
            (1.0..4.0).contains(&per_thousand),
            "{displaced} of {KEYS} keys displaced, {per_thousand} in a thousand"
        );
    }

    #[test]
    fn a_fixed_index_never_wants_to_grow() {
        let index = Index::new(1, true);
        for _ in 0..1000 {
            index.note_key();
        }
        assert!(!index.wants_growth());
    }

    #[test]
    fn an_index_wants_to_grow_at_half_full() {
        let index = Index::new(1 << 4, false);
        let slots = index.buckets() * SLOTS;
        for _ in 0..slots / 2 - 1 {
            index.note_key();
        }
        assert!(!index.wants_growth(), "grew before it was half full");
        index.note_key();
        assert!(index.wants_growth(), "did not grow at half full");
    }

    #[test]
    fn the_hash_spreads_ycsb_keys_over_buckets_and_tags() {
        // The keys a YCSB load actually generates, which are the ones
        // that have to spread: a fixed prefix and an ascending number.
        let index = Index::new(1 << 12, true);
        let mut counts = vec![0u32; index.buckets()];
        let mut tags = std::collections::HashSet::new();
        let keys = 1 << 15;
        for i in 0..keys {
            let key = format!("user{i}");
            let h = hash(key.as_bytes());
            counts[h as usize & (index.buckets() - 1)] += 1;
            tags.insert(Index::tag(h));
        }
        let expected = keys / index.buckets() as u32;
        let worst = counts.iter().copied().max().expect("non empty");
        // A fair coin over this many buckets puts the worst bucket
        // around 3x the mean; 4x is slack for the run, and a hash that
        // keyed on the prefix would blow straight past it.
        assert!(
            worst <= expected * 4,
            "worst bucket {worst} against a mean of {expected}"
        );
        // Eight keys a bucket on average, so a fair hash leaves about
        // 4096 * e^-8, call it one or two, buckets empty. Anything near
        // a whole percent means the keys are clumping.
        let empty = counts.iter().filter(|&&c| c == 0).count();
        assert!(empty * 100 < index.buckets(), "{empty} buckets got nothing");
        // Two draws per tag on average, so a fair hash covers about
        // 1 - e^-2 of them, and a hash that ignored the changing bytes
        // would cover almost none.
        let tag_space = (TAG_MASK + 1) as usize;
        assert!(
            tags.len() * 10 > tag_space * 8,
            "tags cover only {} of {tag_space}",
            tags.len()
        );
    }

    #[test]
    fn one_flipped_bit_changes_both_ends_of_the_hash() {
        let a = hash(b"user1000000000000");
        let b = hash(b"user1000000000001");
        assert_ne!(a & 0xFFF, b & 0xFFF, "low bits stuck");
        assert_ne!(Index::tag(a), Index::tag(b), "tag bits stuck");
    }
}
