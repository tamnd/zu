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

use std::sync::atomic::{AtomicU64, Ordering};

/// Entries in one bucket, which is one cacheline.
pub const SLOTS: usize = 8;

const TENTATIVE: u64 = 1 << 63;
const FOREIGN: u64 = 1 << 62;
const TAG_SHIFT: u32 = 48;
const TAG_MASK: u64 = 0x3FFF;
const ADDRESS_MASK: u64 = (1 << 48) - 1;

/// The entry an empty slot holds.
pub const EMPTY: u64 = 0;

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

pub struct Index {
    buckets: Box<[Bucket]>,
    mask: usize,
}

impl Index {
    /// Sizes the table to at least `buckets`, rounded up to a power of
    /// two. Sizing is a hint from the caller in this version; growing
    /// under an exclusive epoch is a later milestone, and until then a
    /// table that fills simply chains in the log.
    pub fn new(buckets: usize) -> Self {
        let count = buckets.max(1).next_power_of_two();
        Self {
            buckets: (0..count).map(|_| Bucket::default()).collect(),
            mask: count - 1,
        }
    }

    pub fn buckets(&self) -> usize {
        self.buckets.len()
    }

    #[inline]
    pub fn bucket(&self, hash: u64) -> &Bucket {
        // The low bits pick the bucket and the top bits are the tag, so
        // the two never overlap however the table is sized.
        &self.buckets[hash as usize & self.mask]
    }

    /// The tag for a hash: 14 bits off the top, independent of the bits
    /// the bucket index used.
    #[inline]
    pub const fn tag(hash: u64) -> u64 {
        (hash >> 50) & TAG_MASK
    }

    /// How many entries are in use, for tests and for reporting the
    /// load factor a benchmark ran at.
    pub fn occupancy(&self) -> usize {
        self.buckets
            .iter()
            .map(|b| {
                b.slots
                    .iter()
                    .filter(|s| s.load(Ordering::Relaxed) != EMPTY)
                    .count()
            })
            .sum()
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

    #[test]
    fn the_hash_spreads_ycsb_keys_over_buckets_and_tags() {
        // The keys a YCSB load actually generates, which are the ones
        // that have to spread: a fixed prefix and an ascending number.
        let index = Index::new(1 << 12);
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
